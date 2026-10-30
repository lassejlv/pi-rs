//! Google Cloud credentials for Vertex AI: the parts of `google-auth-library` that
//! `@google/genai` relies on (Application Default Credentials, service-account JWT token
//! minting with RS256, authorized-user refresh tokens, impersonated service accounts and the
//! GCE metadata server).
//!
//! Resolution order (`GoogleAuth.getApplicationDefault`): an explicit key file
//! (`GOOGLE_APPLICATION_CREDENTIALS` from the provider env), else the
//! `GOOGLE_APPLICATION_CREDENTIALS` process env, the gcloud well-known file
//! (`~/.config/gcloud/application_default_credentials.json`, `%APPDATA%\gcloud\...` on Windows),
//! and finally the GCE metadata server. Workload and workforce identity federation
//! (`external_account`, `external_account_authorized_user`) live in
//! [`super::google_external_account`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::Sha256;

use super::google_external_account::{
    ExternalAccountAuthorizedUserConfig, ExternalAccountConfig, auth_api_client_header, auth_user_agent,
    GaxiosRetry, external_account_access_token, external_account_authorized_user_token, gaxios_error_message,
    gaxios_send,
};

/// OAuth scope Vertex AI requires.
pub const VERTEX_AI_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Google's OAuth 2.0 token endpoint.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const DEFAULT_UNIVERSE: &str = "googleapis.com";
/// Tokens are refreshed this long before they expire.
const EAGER_REFRESH_THRESHOLD_MS: i64 = 5 * 60 * 1000;

/// `GoogleAuthExceptionMessages.NO_ADC_FOUND`.
pub const NO_ADC_FOUND: &str =
    "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";

static TOKEN_URL_OVERRIDE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at_ms: Option<i64>,
}

/// Point token requests at another endpoint (tests only). `None` restores the default.
#[doc(hidden)]
pub fn set_google_token_url_override(url: Option<String>) {
    *TOKEN_URL_OVERRIDE.lock() = url;
}

fn token_url() -> String {
    TOKEN_URL_OVERRIDE.lock().clone().unwrap_or_else(|| GOOGLE_TOKEN_URL.to_string())
}

/// A parsed Google credential.
#[derive(Debug, Clone, PartialEq)]
pub enum GoogleCredentials {
    /// `"type": "service_account"` (also the default for files without a type).
    ServiceAccount {
        client_email: String,
        private_key: String,
        private_key_id: Option<String>,
        quota_project_id: Option<String>,
        universe_domain: String,
    },
    /// `"type": "authorized_user"` (what `gcloud auth application-default login` writes).
    AuthorizedUser { client_id: String, client_secret: String, refresh_token: String, quota_project_id: Option<String> },
    /// `"type": "impersonated_service_account"`.
    Impersonated {
        source: Box<GoogleCredentials>,
        service_account_impersonation_url: String,
        delegates: Vec<String>,
        quota_project_id: Option<String>,
    },
    /// `"type": "external_account"` (workload/workforce identity federation).
    ExternalAccount(Box<ExternalAccountConfig>),
    /// `"type": "external_account_authorized_user"` (workforce identity federation user).
    ExternalAccountAuthorizedUser(Box<ExternalAccountAuthorizedUserConfig>),
    /// The GCE metadata server.
    Compute,
}

fn str_field(json: &Value, key: &str) -> Option<String> {
    json.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

impl GoogleCredentials {
    /// Parse a credential JSON object (`GoogleAuth.fromJSON`).
    pub fn from_json(json: &Value) -> anyhow::Result<GoogleCredentials> {
        if !json.is_object() {
            anyhow::bail!("Must pass in a JSON object containing the service account auth settings.");
        }
        match json.get("type").and_then(Value::as_str) {
            Some("authorized_user") => {
                let client_id = str_field(json, "client_id");
                let client_secret = str_field(json, "client_secret");
                let refresh_token = str_field(json, "refresh_token");
                let (Some(client_id), Some(client_secret), Some(refresh_token)) = (client_id, client_secret, refresh_token)
                else {
                    if str_field(json, "client_id").is_none() {
                        anyhow::bail!("The incoming JSON object does not contain a client_id field");
                    }
                    if str_field(json, "client_secret").is_none() {
                        anyhow::bail!("The incoming JSON object does not contain a client_secret field");
                    }
                    anyhow::bail!("The incoming JSON object does not contain a refresh_token field");
                };
                Ok(GoogleCredentials::AuthorizedUser {
                    client_id,
                    client_secret,
                    refresh_token,
                    quota_project_id: str_field(json, "quota_project_id"),
                })
            }
            Some("impersonated_service_account") => {
                let Some(source) = json.get("source_credentials").filter(|s| s.is_object()) else {
                    anyhow::bail!("The incoming JSON object does not contain a source_credentials field");
                };
                let Some(url) = str_field(json, "service_account_impersonation_url") else {
                    anyhow::bail!("The incoming JSON object does not contain a service_account_impersonation_url field");
                };
                if url.chars().count() > 256 {
                    anyhow::bail!("Target principal is too long: {url}");
                }
                let target = regex::Regex::new(r"([^/]+):(generateAccessToken|generateIdToken)$").expect("valid regex");
                if !target.is_match(&url) {
                    anyhow::bail!("Cannot extract target principal from {url}");
                }
                let delegates = json
                    .get("delegates")
                    .and_then(Value::as_array)
                    .map(|d| d.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                Ok(GoogleCredentials::Impersonated {
                    source: Box::new(GoogleCredentials::from_json(source)?),
                    service_account_impersonation_url: url,
                    delegates,
                    quota_project_id: str_field(json, "quota_project_id"),
                })
            }
            Some("external_account") => {
                Ok(GoogleCredentials::ExternalAccount(Box::new(ExternalAccountConfig::from_json(json)?)))
            }
            Some("external_account_authorized_user") => Ok(GoogleCredentials::ExternalAccountAuthorizedUser(Box::new(
                ExternalAccountAuthorizedUserConfig::from_json(json),
            ))),
            _ => {
                let Some(client_email) = str_field(json, "client_email") else {
                    anyhow::bail!("The incoming JSON object does not contain a client_email field");
                };
                let Some(private_key) = str_field(json, "private_key") else {
                    anyhow::bail!("The incoming JSON object does not contain a private_key field");
                };
                Ok(GoogleCredentials::ServiceAccount {
                    client_email,
                    private_key,
                    private_key_id: str_field(json, "private_key_id"),
                    quota_project_id: str_field(json, "quota_project_id"),
                    universe_domain: str_field(json, "universe_domain").unwrap_or_else(|| DEFAULT_UNIVERSE.to_string()),
                })
            }
        }
    }

    /// Read a credential file (`_getApplicationCredentialsFromFilePath`).
    pub fn from_file(path: &Path) -> anyhow::Result<GoogleCredentials> {
        if path.as_os_str().is_empty() {
            anyhow::bail!("The file path is invalid.");
        }
        let resolved = super::google_external_account::resolve_regular_file(&path.to_string_lossy())?;
        let text = std::fs::read_to_string(&resolved)?;
        let json: Value = crate::utils::js_json::js_json_parse(&text).map_err(anyhow::Error::msg)?;
        GoogleCredentials::from_json(&json)
    }

    fn quota_project_id(&self) -> Option<&str> {
        match self {
            GoogleCredentials::ServiceAccount { quota_project_id, .. }
            | GoogleCredentials::AuthorizedUser { quota_project_id, .. }
            | GoogleCredentials::Impersonated { quota_project_id, .. } => quota_project_id.as_deref(),
            GoogleCredentials::ExternalAccount(config) => config.quota_project_id.as_deref(),
            GoogleCredentials::ExternalAccountAuthorizedUser(config) => config.quota_project_id.as_deref(),
            GoogleCredentials::Compute => None,
        }
    }
}

/// The gcloud well-known ADC file location.
pub fn well_known_credentials_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
    }?;
    Some(base.join("gcloud").join("application_default_credentials.json"))
}

fn gce_metadata_host() -> String {
    std::env::var("GCE_METADATA_HOST")
        .or_else(|_| std::env::var("GCE_METADATA_IP"))
        .unwrap_or_else(|_| "169.254.169.254".to_string())
}

/// `gcpMetadata.getGCPResidency() || isAvailable()`.
async fn is_gce(client: &reqwest::Client) -> bool {
    if std::env::var_os("GCE_METADATA_HOST").is_some() || std::env::var_os("GCE_METADATA_IP").is_some() {
        return true;
    }
    if cfg!(target_os = "linux") {
        for file in ["/sys/class/dmi/id/bios_vendor", "/sys/class/dmi/id/product_name"] {
            if std::fs::read_to_string(file).is_ok_and(|s| s.contains("Google")) {
                return true;
            }
        }
    }
    if std::env::var("METADATA_SERVER_DETECTION").is_ok_and(|v| v == "none") {
        return false;
    }
    let url = format!("http://{}/computeMetadata/v1/instance", gce_metadata_host());
    let probe = client.get(url).header("Metadata-Flavor", "Google").timeout(Duration::from_millis(3000)).send().await;
    probe.is_ok_and(|r| r.headers().get("metadata-flavor").is_some_and(|v| v == "Google"))
}

/// Resolve credentials: an explicit key file, else Application Default Credentials.
pub async fn resolve_credentials(key_filename: Option<&str>, client: &reqwest::Client) -> anyhow::Result<GoogleCredentials> {
    if let Some(path) = key_filename.filter(|p| !p.is_empty()) {
        return GoogleCredentials::from_file(Path::new(path));
    }
    let env_path = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .or_else(|| std::env::var("google_application_credentials").ok())
        .filter(|p| !p.is_empty());
    if let Some(path) = env_path {
        return GoogleCredentials::from_file(Path::new(&path)).map_err(|e| {
            anyhow::anyhow!(
                "Unable to read the credential file specified by the GOOGLE_APPLICATION_CREDENTIALS environment variable: {e}"
            )
        });
    }
    if let Some(path) = well_known_credentials_path().filter(|p| p.exists()) {
        return GoogleCredentials::from_file(&path);
    }
    if is_gce(client).await {
        return Ok(GoogleCredentials::Compute);
    }
    anyhow::bail!(NO_ADC_FOUND)
}

/// Load a PEM RSA private key (PKCS#8 or PKCS#1).
fn parse_private_key(pem: &str) -> anyhow::Result<rsa::RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    let pem = pem.replace("\\n", "\n");
    rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| anyhow::anyhow!("Invalid service account private key: {e}"))
}

/// Sign a JWT with RS256 (`jws.sign({ header, payload, secret })`).
pub fn sign_jwt_rs256(header: &Value, payload: &Value, private_key_pem: &str) -> anyhow::Result<String> {
    use rsa::signature::{SignatureEncoding, Signer};
    let key = parse_private_key(private_key_pem)?;
    let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(key);
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(header)?),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload)?)
    );
    let signature = signing_key.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

fn now_secs() -> i64 {
    crate::utils::now_ms() / 1000
}

/// The JWT-bearer assertion for a service account (`gtoken` payload).
pub fn service_account_assertion(client_email: &str, private_key: &str, scope: &str) -> anyhow::Result<String> {
    let iat = now_secs();
    let payload = json!({ "iss": client_email, "scope": scope, "aud": GOOGLE_TOKEN_URL, "exp": iat + 3600, "iat": iat });
    sign_jwt_rs256(&json!({ "alg": "RS256" }), &payload, private_key)
}

/// A self-signed JWT access token (`JWTAccess`), used outside the default universe.
fn self_signed_jwt(client_email: &str, private_key: &str, key_id: Option<&str>, scope: &str) -> anyhow::Result<String> {
    let iat = now_secs();
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(kid) = key_id {
        header["kid"] = json!(kid);
    }
    let payload = json!({ "iss": client_email, "sub": client_email, "scope": scope, "exp": iat + 3600, "iat": iat });
    sign_jwt_rs256(&header, &payload, private_key)
}

/// How a token endpoint failure is reported.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenErrorStyle {
    /// `gtoken` (service accounts): `<error>: <error_description>`.
    GToken,
    /// `OAuth2Client.refreshTokenNoCache` (authorized users): the Gaxios message.
    RefreshToken,
}

async fn post_token_form(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    style: TokenErrorStyle,
) -> anyhow::Result<(String, Option<i64>)> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form.iter()).finish();
    let mut request = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded;charset=UTF-8")
        .header("x-goog-api-client", auth_api_client_header())
        .header("User-Agent", auth_user_agent());
    if style == TokenErrorStyle::GToken {
        request = request.header("accept", "application/json");
    }
    // gtoken and `OAuth2Client.refreshTokenNoCache` both retry like Gaxios' auth requests.
    let response = gaxios_send(request.body(body), Some(GaxiosRetry::AUTH))
        .await
        .map_err(|e| anyhow::anyhow!("request to {url} failed, reason: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let mut message = gaxios_error_message(status.as_u16(), &text, true);
        match style {
            TokenErrorStyle::GToken => {
                if let Some(error) = data.get("error").filter(|e| !e.is_null() && *e != &json!(false) && *e != &json!("")) {
                    let text = |v: Option<&Value>| match v {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => "undefined".to_string(),
                    };
                    message = format!("{}: {}", text(Some(error)), text(data.get("error_description")));
                }
            }
            TokenErrorStyle::RefreshToken => {
                let reauth = data
                    .get("error_description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| d.to_lowercase().contains("reauth"));
                if message == "invalid_grant" && reauth {
                    message = data.to_string();
                }
            }
        }
        // `OAuth2Client.getRequestMetadataAsync`.
        if matches!(status.as_u16(), 403 | 404) {
            message = format!("Could not refresh access token: {message}");
        }
        anyhow::bail!(message);
    }
    let Some(token) = data.get("access_token").and_then(Value::as_str) else {
        anyhow::bail!("No access token in token response");
    };
    let expires_at = data.get("expires_in").and_then(Value::as_f64).map(|s| crate::utils::now_ms() + (s * 1000.0) as i64);
    Ok((token.to_string(), expires_at))
}

fn fetch_access_token<'a>(
    credentials: &'a GoogleCredentials,
    scope: &'a str,
    client: &'a reqwest::Client,
) -> futures::future::BoxFuture<'a, anyhow::Result<(String, Option<i64>)>> {
    crate::utils::wasm::boxed(async move {
        match credentials {
            GoogleCredentials::ServiceAccount { client_email, private_key, private_key_id, universe_domain, .. } => {
                if universe_domain != DEFAULT_UNIVERSE {
                    let token = self_signed_jwt(client_email, private_key, private_key_id.as_deref(), scope)?;
                    return Ok((token, Some(crate::utils::now_ms() + 3600 * 1000)));
                }
                let assertion = service_account_assertion(client_email, private_key, scope)?;
                post_token_form(
                    client,
                    &token_url(),
                    &[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)],
                    TokenErrorStyle::GToken,
                )
                .await
            }
            GoogleCredentials::AuthorizedUser { client_id, client_secret, refresh_token, .. } => {
                post_token_form(
                    client,
                    &token_url(),
                    &[
                        ("refresh_token", refresh_token),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("grant_type", "refresh_token"),
                    ],
                    TokenErrorStyle::RefreshToken,
                )
                .await
            }
            GoogleCredentials::Impersonated { source, service_account_impersonation_url, delegates, .. } => {
                let (source_token, _) = fetch_access_token(source, scope, client).await?;
                let request = client
                    .post(service_account_impersonation_url)
                    .bearer_auth(source_token)
                    .header("x-goog-api-client", auth_api_client_header())
                    .header("User-Agent", auth_user_agent())
                    .json(&json!({ "delegates": delegates, "scope": [scope], "lifetime": "3600s" }));
                // `Impersonated.RETRY_CONFIG`.
                let response = gaxios_send(request, Some(GaxiosRetry::AUTH)).await?;
                let status = response.status();
                let data: Value = response.json().await.unwrap_or(Value::Null);
                if !status.is_success() {
                    let message = data
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Request failed with status code {}", status.as_u16()));
                    anyhow::bail!("unable to impersonate: {message}");
                }
                let Some(token) = data.get("accessToken").and_then(Value::as_str) else {
                    anyhow::bail!("unable to impersonate: no access token returned");
                };
                let expires_at = data
                    .get("expireTime")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_ms);
                Ok((token.to_string(), expires_at))
            }
            GoogleCredentials::ExternalAccount(config) => external_account_access_token(config, scope, client).await,
            GoogleCredentials::ExternalAccountAuthorizedUser(config) => {
                external_account_authorized_user_token(config, client).await
            }
            GoogleCredentials::Compute => {
                let url = format!(
                    "http://{}/computeMetadata/v1/instance/service-accounts/default/token?scopes={}",
                    gce_metadata_host(),
                    url::form_urlencoded::byte_serialize(scope.as_bytes()).collect::<String>()
                );
                // gcp-metadata: `retryConfig: { noResponseRetries: 3 }`.
                let response = gaxios_send(client.get(url).header("Metadata-Flavor", "Google"), Some(GaxiosRetry::METADATA)).await?;
                if !response.status().is_success() {
                    anyhow::bail!("Could not refresh access token: metadata server returned {}", response.status().as_u16());
                }
                let data: Value = response.json().await?;
                let Some(token) = data.get("access_token").and_then(Value::as_str) else {
                    anyhow::bail!("Could not refresh access token: no access token returned by the metadata server");
                };
                let expires_at =
                    data.get("expires_in").and_then(Value::as_f64).map(|s| crate::utils::now_ms() + (s * 1000.0) as i64);
                Ok((token.to_string(), expires_at))
            }
        }
    })
}

/// Parse an RFC 3339 UTC timestamp (`2026-01-01T00:00:00Z`, optional fraction) to epoch ms.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim_end_matches('Z');
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, s) = (t.next()??, t.next()??, t.next()??);
    let millis = format!("{frac:0<3}").get(..3)?.parse::<i64>().ok()?;
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + s) * 1000 + millis)
}

fn quota_headers(credentials: &GoogleCredentials, token: &str) -> Vec<(String, String)> {
    let mut headers = vec![("authorization".to_string(), format!("Bearer {token}"))];
    let quota = std::env::var("GOOGLE_CLOUD_QUOTA_PROJECT")
        .ok()
        .filter(|q| !q.is_empty())
        .or_else(|| credentials.quota_project_id().map(str::to_string));
    if let Some(quota) = quota {
        headers.push(("x-goog-user-project".to_string(), quota));
    }
    headers
}

/// Request headers for `credentials` with a freshly fetched token: `authorization: Bearer
/// <token>` and, when a quota project is configured (`quota_project_id` or
/// `GOOGLE_CLOUD_QUOTA_PROJECT`), `x-goog-user-project`.
pub async fn get_request_headers(
    credentials: &GoogleCredentials,
    scope: &str,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<(String, String)>> {
    let (token, _) = fetch_access_token(credentials, scope, client).await?;
    Ok(quota_headers(credentials, &token))
}

/// One `GoogleAuth` instance (what `@google/genai` creates per client, i.e. per pi request):
/// credentials resolve on first use (`getClient` caches them only on success) and the access
/// token is reused until it is within five minutes of expiring. Nothing is shared across
/// instances, so every request re-reads the credentials and fetches a new token, as in TS.
pub struct GoogleAuth {
    key_filename: Option<String>,
    state: tokio::sync::Mutex<Option<(GoogleCredentials, Option<CachedToken>)>>,
}

impl GoogleAuth {
    pub fn new(key_filename: Option<String>) -> Self {
        Self { key_filename, state: tokio::sync::Mutex::new(None) }
    }

    /// `GoogleAuth.getRequestHeaders()`.
    pub async fn get_request_headers(&self, scope: &str, client: &reqwest::Client) -> anyhow::Result<Vec<(String, String)>> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            let credentials = resolve_credentials(self.key_filename.as_deref(), client).await?;
            *state = Some((credentials, None));
        }
        let (credentials, cached) = state.as_mut().expect("credentials resolved above");
        let fresh = cached.as_ref().filter(|c| {
            c.expires_at_ms.is_none_or(|e| e - EAGER_REFRESH_THRESHOLD_MS > crate::utils::now_ms())
        });
        let token = match fresh {
            Some(cached) => cached.token.clone(),
            None => {
                let (token, expires_at_ms) = fetch_access_token(credentials, scope, client).await?;
                *cached = Some(CachedToken { token: token.clone(), expires_at_ms });
                token
            }
        };
        Ok(quota_headers(credentials, &token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credential_files() {
        let user = GoogleCredentials::from_json(&json!({
            "type": "authorized_user", "client_id": "id", "client_secret": "secret", "refresh_token": "rt",
            "quota_project_id": "q"
        }))
        .unwrap();
        assert_eq!(user.quota_project_id(), Some("q"));
        let missing = GoogleCredentials::from_json(&json!({ "type": "service_account", "private_key": "k" })).unwrap_err();
        assert_eq!(missing.to_string(), "The incoming JSON object does not contain a client_email field");
        let external = GoogleCredentials::from_json(&json!({ "type": "external_account" })).unwrap_err();
        assert_eq!(external.to_string(), "Cannot read properties of undefined (reading 'match')");
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_rfc3339_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
    }
}
