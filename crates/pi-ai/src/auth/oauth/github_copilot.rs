//! GitHub Copilot OAuth flow (port of `auth/oauth/github-copilot.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::http::{OAuthFetch, OAuthHttpRequest, OAuthHttpResponse, current_oauth_fetch, send};
use crate::auth::types::{
    AuthEvent, AuthPrompt, AuthPromptKind, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
    ProviderAuthInteraction,
};
use crate::models_generated::CATALOG;

fn client_id() -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("SXYxLmI1MDdhMDhjODdlY2ZlOTg=")
        .unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
const COPILOT_API_VERSION: &str = "2026-06-01";
const INDIVIDUAL_BASE_URL: &str = "https://api.individual.githubcopilot.com";

#[derive(Debug, Clone)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: f64,
}

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_retries: u32,
    max_elapsed_ms: u64,
}

/// Hostname of a GitHub Enterprise URL or domain; `None` when blank or invalid.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = if trimmed.contains("://") {
        url::Url::parse(trimmed)
    } else {
        url::Url::parse(&format!("https://{trimmed}"))
    };
    url.ok().and_then(|u| u.host_str().map(str::to_string))
}

struct Urls {
    device_code_url: String,
    access_token_url: String,
    copilot_token_url: String,
}

fn get_urls(domain: &str) -> Urls {
    Urls {
        device_code_url: format!("https://{domain}/login/device/code"),
        access_token_url: format!("https://{domain}/login/oauth/access_token"),
        copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
    }
}

/// `proxy-ep=proxy.xxx` in a Copilot token, as an API base URL (`https://api.xxx`).
fn get_base_url_from_token(token: &str) -> Option<String> {
    let start = token.find("proxy-ep=")? + "proxy-ep=".len();
    let proxy_host: String = token[start..].chars().take_while(|c| *c != ';').collect();
    if proxy_host.is_empty() {
        return None;
    }
    let api_host = proxy_host
        .strip_prefix("proxy.")
        .map(|rest| format!("api.{rest}"))
        .unwrap_or(proxy_host);
    Some(format!("https://{api_host}"))
}

/// The Copilot API base URL for a token and optional enterprise domain.
pub fn get_github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(url) = token.and_then(get_base_url_from_token) {
        return url;
    }
    match enterprise_domain.filter(|d| !d.is_empty()) {
        Some(domain) => format!("https://copilot-api.{domain}"),
        None => INDIVIDUAL_BASE_URL.to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ModelCatalog {
    available_model_ids: Vec<String>,
    policy_model_ids: Vec<String>,
}

fn parse_model_catalog(raw: &Value, allow_policy_fallback: bool) -> anyhow::Result<ModelCatalog> {
    let Some(Value::Array(data)) = raw.as_object().and_then(|o| o.get("data")) else {
        anyhow::bail!("Invalid Copilot models response");
    };
    struct AccountModel {
        id: String,
        picker_enabled: bool,
        policy_state: Option<String>,
    }
    let account_models: Vec<AccountModel> = data
        .iter()
        .filter_map(|item| {
            let item = item.as_object()?;
            let id = item.get("id")?.as_str()?.to_string();
            let supports = item.get("capabilities").and_then(|c| c.get("supports"));
            if supports.and_then(|s| s.get("tool_calls")) == Some(&Value::Bool(false)) {
                return None;
            }
            Some(AccountModel {
                id,
                picker_enabled: item.get("model_picker_enabled") == Some(&Value::Bool(true)),
                policy_state: item
                    .get("policy")
                    .and_then(|p| p.get("state"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    let picker_ids: Vec<String> = account_models
        .iter()
        .filter(|m| m.picker_enabled && m.policy_state.as_deref() != Some("disabled"))
        .map(|m| m.id.clone())
        .collect();
    let use_policy_fallback = allow_policy_fallback && picker_ids.is_empty();
    let available_model_ids = if !picker_ids.is_empty() || !allow_policy_fallback {
        picker_ids
    } else {
        account_models
            .iter()
            .filter(|m| m.policy_state.as_deref() == Some("enabled"))
            .map(|m| m.id.clone())
            .collect()
    };
    let known = CATALOG.chat.get("github-copilot");
    let policy_model_ids = account_models
        .iter()
        .filter(|m| {
            m.policy_state.as_deref() == Some("unconfigured")
                && known.is_some_and(|k| k.contains_key(&m.id))
                && (m.picker_enabled || use_policy_fallback)
        })
        .map(|m| m.id.clone())
        .collect();
    Ok(ModelCatalog {
        available_model_ids,
        policy_model_ids,
    })
}

fn with_copilot_headers(mut request: OAuthHttpRequest) -> OAuthHttpRequest {
    for (name, value) in COPILOT_HEADERS {
        request = request.header(name, value);
    }
    request
}

fn js_parse_float(text: &str) -> f64 {
    crate::utils::provider_retry::js_parse_float(text)
}

fn parse_http_date_ms(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<i64> = parts[4].split(':').filter_map(|p| p.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2]) * 1000) as f64)
}

async fn fetch_with_rate_limit_retry(
    fetch: &OAuthFetch,
    request: OAuthHttpRequest,
    signal: &CancellationToken,
    policy: RetryPolicy,
) -> anyhow::Result<OAuthHttpResponse> {
    let budget = if policy.max_retries > 0 && policy.max_elapsed_ms > 0 {
        Some(crate::utils::wasm::Instant::now() + std::time::Duration::from_millis(policy.max_elapsed_ms))
    } else {
        None
    };
    let request_signal = signal.child_token();
    let budget_guard = budget.map(|deadline| {
        let token = request_signal.clone();
        crate::utils::wasm::spawn(async move {
            crate::utils::wasm::sleep_until(deadline).await;
            token.cancel();
        })
    });
    let result = async {
        let mut retry: u32 = 0;
        loop {
            let response = send(fetch, request.clone(), Some(&request_signal), Some(5000)).await?;
            if response.status != 429 || retry == policy.max_retries {
                return Ok(response);
            }
            let mut delay_ms = 500.0 * 2f64.powi(retry as i32);
            if let Some(retry_after) = response.header("retry-after") {
                let seconds = js_parse_float(retry_after);
                delay_ms = if seconds.is_nan() {
                    parse_http_date_ms(retry_after)
                        .map(|d| d - crate::utils::now_ms() as f64)
                        .unwrap_or(f64::NAN)
                } else {
                    seconds * 1000.0
                };
                if !delay_ms.is_finite() {
                    return Ok(response);
                }
            }
            let delay_ms = delay_ms.max(0.0);
            if let Some(deadline) = budget {
                let remaining = deadline
                    .saturating_duration_since(crate::utils::wasm::Instant::now())
                    .as_secs_f64()
                    * 1000.0;
                if delay_ms >= remaining {
                    return Ok(response);
                }
            }
            crate::utils::sleep::sleep(delay_ms as u64, &request_signal).await?;
            retry += 1;
        }
    }
    .await;
    if let Some(guard) = budget_guard {
        guard.abort();
    }
    result
}

async fn fetch_github_copilot_models(
    fetch: &OAuthFetch,
    copilot_token: &str,
    enterprise_domain: Option<&str>,
    signal: &CancellationToken,
    policy: RetryPolicy,
) -> anyhow::Result<ModelCatalog> {
    let base_url = get_github_copilot_base_url(Some(copilot_token), enterprise_domain);
    // Some Individual accounts return false for every picker flag despite explicit enabled
    // policies. Limit the fallback to that endpoint.
    let allow_policy_fallback = base_url == INDIVIDUAL_BASE_URL;
    let request = with_copilot_headers(
        OAuthHttpRequest::get(format!("{base_url}/models"))
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {copilot_token}")),
    )
    .header("X-GitHub-Api-Version", COPILOT_API_VERSION);
    let response = fetch_with_rate_limit_retry(fetch, request, signal, policy).await?;
    if !response.ok() {
        anyhow::bail!(
            "{} {}: {}",
            response.status,
            response.status_text,
            response.body
        );
    }
    parse_model_catalog(&response.json()?, allow_policy_fallback)
}

async fn fetch_json(
    fetch: &OAuthFetch,
    request: OAuthHttpRequest,
    signal: &CancellationToken,
) -> anyhow::Result<Value> {
    let response = send(fetch, request, Some(signal), None).await?;
    if !response.ok() {
        anyhow::bail!(
            "{} {}: {}",
            response.status,
            response.status_text,
            response.body
        );
    }
    response.json()
}

async fn start_device_flow(
    fetch: &OAuthFetch,
    domain: &str,
    signal: &CancellationToken,
) -> anyhow::Result<DeviceCodeResponse> {
    let urls = get_urls(domain);
    let request = OAuthHttpRequest::post(urls.device_code_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .form(&[("client_id", &client_id()), ("scope", "read:user")]);
    let data = fetch_json(fetch, request, signal).await?;
    let Some(object) = data.as_object() else {
        anyhow::bail!("Invalid device code response")
    };
    let device_code = object.get("device_code").and_then(Value::as_str);
    let user_code = object.get("user_code").and_then(Value::as_str);
    let verification_uri = object.get("verification_uri").and_then(Value::as_str);
    let interval = object.get("interval");
    let expires_in = object.get("expires_in").and_then(Value::as_f64);
    let interval_valid = interval.is_none_or(|i| i.is_number());
    let (Some(device_code), Some(user_code), Some(verification_uri), true, Some(expires_in)) = (
        device_code,
        user_code,
        verification_uri,
        interval_valid,
        expires_in,
    ) else {
        anyhow::bail!("Invalid device code response fields");
    };
    // The verification URI is opened in the user's browser; force it to be an http(s) URL.
    let parsed = url::Url::parse(verification_uri)
        .map_err(|_| anyhow::anyhow!("Untrusted verification_uri in device code response"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("Untrusted verification_uri in device code response");
    }
    Ok(DeviceCodeResponse {
        device_code: device_code.to_string(),
        user_code: user_code.to_string(),
        verification_uri: parsed.to_string(),
        interval: interval.and_then(Value::as_f64),
        expires_in,
    })
}

async fn poll_for_github_access_token(
    fetch: &OAuthFetch,
    domain: &str,
    device: &DeviceCodeResponse,
    signal: &CancellationToken,
) -> anyhow::Result<String> {
    let urls = get_urls(domain);
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: true,
            signal: signal.clone(),
        },
        || async {
            let request = OAuthHttpRequest::post(urls.access_token_url.clone())
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", "GitHubCopilotChat/0.35.0")
                .form(&[
                    ("client_id", &client_id()),
                    ("device_code", &device.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]);
            let raw = fetch_json(fetch, request, signal).await?;
            if let Some(token) = raw.get("access_token").and_then(Value::as_str) {
                return Ok(OAuthDeviceCodePollResult::Complete(token.to_string()));
            }
            if let Some(error) = raw.get("error").and_then(Value::as_str) {
                return Ok(match error {
                    "authorization_pending" => OAuthDeviceCodePollResult::Pending,
                    "slow_down" => OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: raw.get("interval").and_then(Value::as_f64),
                    },
                    _ => {
                        let suffix = raw
                            .get("error_description")
                            .and_then(Value::as_str)
                            .filter(|d| !d.is_empty())
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default();
                        OAuthDeviceCodePollResult::Failed {
                            message: format!("Device flow failed: {error}{suffix}"),
                        }
                    }
                });
            }
            Ok(OAuthDeviceCodePollResult::Failed {
                message: "Invalid device token response".into(),
            })
        },
    )
    .await
}

async fn refresh_github_copilot_access_token(
    fetch: &OAuthFetch,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    signal: &CancellationToken,
) -> anyhow::Result<OAuthCredential> {
    let domain = enterprise_domain
        .filter(|d| !d.is_empty())
        .unwrap_or("github.com");
    let urls = get_urls(domain);
    let request = with_copilot_headers(
        OAuthHttpRequest::get(urls.copilot_token_url)
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {refresh_token}")),
    );
    let raw = fetch_json(fetch, request, signal).await?;
    if !raw.is_object() {
        anyhow::bail!("Invalid Copilot token response");
    }
    let (Some(token), Some(expires_at)) = (
        raw.get("token").and_then(Value::as_str),
        raw.get("expires_at").and_then(Value::as_f64),
    ) else {
        anyhow::bail!("Invalid Copilot token response fields");
    };
    let mut extra = Map::new();
    if let Some(domain) = enterprise_domain {
        extra.insert("enterpriseUrl".into(), json!(domain));
    }
    Ok(OAuthCredential {
        refresh: refresh_token.to_string(),
        access: token.to_string(),
        expires: (expires_at * 1000.0 - 5.0 * 60.0 * 1000.0) as i64,
        extra,
    })
}

async fn refresh_github_copilot_token(
    fetch: &OAuthFetch,
    refresh_token: &str,
    enterprise_domain: Option<&str>,
    signal: &CancellationToken,
) -> anyhow::Result<OAuthCredential> {
    let mut credential =
        refresh_github_copilot_access_token(fetch, refresh_token, enterprise_domain, signal)
            .await?;
    let catalog = fetch_github_copilot_models(
        fetch,
        &credential.access,
        enterprise_domain,
        signal,
        RetryPolicy {
            max_retries: 0,
            max_elapsed_ms: 0,
        },
    )
    .await?;
    credential.extra.insert(
        "availableModelIds".into(),
        json!(catalog.available_model_ids),
    );
    Ok(credential)
}

/// Enable a model for the account (required for some models before use).
async fn enable_github_copilot_model(
    fetch: &OAuthFetch,
    token: &str,
    model_id: &str,
    enterprise_domain: Option<&str>,
    signal: &CancellationToken,
) -> anyhow::Result<bool> {
    let base_url = get_github_copilot_base_url(Some(token), enterprise_domain);
    let request = with_copilot_headers(
        OAuthHttpRequest::post(format!("{base_url}/models/{model_id}/policy"))
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {token}")),
    )
    .header("openai-intent", "chat-policy")
    .header("x-interaction-type", "chat-policy")
    .body(json!({ "state": "enabled" }).to_string());
    let response = match fetch_with_rate_limit_retry(
        fetch,
        request,
        signal,
        RetryPolicy {
            max_retries: 2,
            max_elapsed_ms: 5000,
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            if signal.is_cancelled() {
                return Err(error);
            }
            return Ok(false);
        }
    };
    if response.status == 429 {
        anyhow::bail!(
            "{} {}: {}",
            response.status,
            response.status_text,
            response.body
        );
    }
    Ok(response.ok())
}

/// Enable the requested models and return the successful ids. Exhausted rate limiting stops the batch.
async fn enable_github_copilot_models(
    fetch: &OAuthFetch,
    token: &str,
    model_ids: &[String],
    enterprise_domain: Option<&str>,
    signal: &CancellationToken,
) -> anyhow::Result<Vec<String>> {
    let mut enabled = Vec::new();
    for model_id in model_ids {
        match enable_github_copilot_model(fetch, token, model_id, enterprise_domain, signal).await {
            Ok(true) => enabled.push(model_id.clone()),
            Ok(false) => {}
            Err(error) => {
                if signal.is_cancelled() {
                    return Err(error);
                }
                break;
            }
        }
    }
    Ok(enabled)
}

async fn login_github_copilot(
    interaction: ProviderAuthInteraction,
) -> anyhow::Result<OAuthCredential> {
    let fetch = current_oauth_fetch();
    let input = interaction
        .interaction
        .prompt(AuthPrompt {
            kind: AuthPromptKind::Text,
            message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
            placeholder: Some("company.ghe.com".into()),
            options: Vec::new(),
            signal: None,
        })
        .await?;
    if interaction.signal.is_cancelled() {
        anyhow::bail!("Login cancelled");
    }
    let trimmed = input.trim();
    let enterprise_domain = normalize_domain(&input);
    if !trimmed.is_empty() && enterprise_domain.is_none() {
        anyhow::bail!("Invalid GitHub Enterprise URL/domain");
    }
    let domain = enterprise_domain
        .clone()
        .unwrap_or_else(|| "github.com".into());
    let signal = &interaction.signal;
    let device = start_device_flow(&fetch, &domain, signal).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: device.interval.map(|i| i as u64),
        expires_in_seconds: Some(device.expires_in as u64),
    });
    let github_access_token =
        poll_for_github_access_token(&fetch, &domain, &device, signal).await?;
    let mut credential = refresh_github_copilot_access_token(
        &fetch,
        &github_access_token,
        enterprise_domain.as_deref(),
        signal,
    )
    .await?;
    let models = fetch_github_copilot_models(
        &fetch,
        &credential.access,
        enterprise_domain.as_deref(),
        signal,
        RetryPolicy {
            max_retries: 2,
            max_elapsed_ms: 5000,
        },
    )
    .await?;
    let mut enabled = Vec::new();
    if !models.policy_model_ids.is_empty() {
        interaction.interaction.notify(AuthEvent::Progress {
            message: "Enabling models...".into(),
        });
        enabled = enable_github_copilot_models(
            &fetch,
            &credential.access,
            &models.policy_model_ids,
            enterprise_domain.as_deref(),
            signal,
        )
        .await?;
    }
    let mut available: Vec<String> = Vec::new();
    for id in models.available_model_ids.into_iter().chain(enabled) {
        if !available.contains(&id) {
            available.push(id);
        }
    }
    credential
        .extra
        .insert("availableModelIds".into(), json!(available));
    Ok(credential)
}

fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    match credential.extra.get("enterpriseUrl") {
        Some(Value::String(url)) if !url.is_empty() => normalize_domain(url),
        _ => None,
    }
}

/// `githubCopilotOAuth`.
pub struct GitHubCopilotOAuth;

#[async_trait]
impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }
    fn is_subscription(&self) -> Option<bool> {
        Some(true)
    }
    async fn login(
        &self,
        interaction: ProviderAuthInteraction,
        _options: Option<LoginOptions>,
    ) -> anyhow::Result<OAuthCredential> {
        login_github_copilot(interaction).await
    }
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> anyhow::Result<OAuthCredential> {
        let domain = copilot_enterprise_domain(&credential);
        refresh_github_copilot_token(
            &current_oauth_fetch(),
            &credential.refresh,
            domain.as_deref(),
            &signal,
        )
        .await
    }
    /// Derive the credential-specific proxy endpoint for each request.
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        let domain = copilot_enterprise_domain(credential);
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: Some(get_github_copilot_base_url(
                Some(&credential.access),
                domain.as_deref(),
            )),
            ..Default::default()
        })
    }
}

pub fn github_copilot_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(GitHubCopilotOAuth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert_eq!(
            get_github_copilot_base_url(
                Some("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;x=y"),
                None
            ),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(
            get_github_copilot_base_url(Some("tid=1"), Some("ghe.example.com")),
            "https://copilot-api.ghe.example.com"
        );
        assert_eq!(get_github_copilot_base_url(None, None), INDIVIDUAL_BASE_URL);
        assert_eq!(
            normalize_domain("https://company.ghe.com/path").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(normalize_domain("  "), None);
    }
}
