//! Workload and workforce identity federation for Google credentials: the `external_account`
//! and `external_account_authorized_user` clients of `google-auth-library` 10.6.2.
//!
//! An `external_account` credential obtains a subject token from its `credential_source` (a file,
//! a URL, an X.509 certificate, an executable, or AWS credentials signing a `GetCallerIdentity`
//! request), exchanges it at the STS endpoint (RFC 8693 token exchange) for a Google access
//! token, and optionally impersonates a service account. `external_account_authorized_user`
//! refreshes a workforce user's refresh token at the STS `oauthtoken` endpoint.
// wasm32 compiles out the native-only transports and sources below.
#![cfg_attr(target_arch = "wasm32", allow(unused_imports))]

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

/// The `google-auth-library` version whose requests this module reproduces.
pub const GOOGLE_AUTH_LIBRARY_VERSION: &str = "10.6.2";
/// `process.version` without the `v`, as reported in `x-goog-api-client`.
pub const NODE_VERSION: &str = "22.19.0";

const STS_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const STS_REQUEST_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_LIFESPAN: u64 = 3600;
const WORKFORCE_AUDIENCE_PATTERN: &str = r"//iam\.googleapis\.com/locations/[^/]+/workforcePools/[^/]+/providers/.+";
const DEFAULT_EXECUTABLE_TIMEOUT_MILLIS: u64 = 30 * 1000;
const MINIMUM_EXECUTABLE_TIMEOUT_MILLIS: u64 = 5 * 1000;
const MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS: u64 = 120 * 1000;
const MAXIMUM_EXECUTABLE_VERSION: i64 = 1;
const SAML_SUBJECT_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:saml2";
const OIDC_SUBJECT_TOKEN_TYPE1: &str = "urn:ietf:params:oauth:token-type:id_token";
const OIDC_SUBJECT_TOKEN_TYPE2: &str = "urn:ietf:params:oauth:token-type:jwt";
const CERTIFICATE_CONFIGURATION_ENV_VARIABLE: &str = "GOOGLE_API_CERTIFICATE_CONFIG";

/// `User-Agent` the auth library's request interceptor adds.
pub fn auth_user_agent() -> String {
    format!("google-api-nodejs-client/{GOOGLE_AUTH_LIBRARY_VERSION}")
}

/// Default `x-goog-api-client` the auth library's request interceptor adds.
pub fn auth_api_client_header() -> String {
    format!("gl-node/{NODE_VERSION}")
}

/// `snakeToCamel`.
fn snake_to_camel(key: &str) -> String {
    let mut out = String::new();
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_'
            && let Some(&next) = chars.peek()
            && next != '_'
        {
            chars.next();
            out.extend(next.to_uppercase());
            continue;
        }
        out.push(c);
    }
    out
}

/// `originalOrCamelOptions(obj).get(key)`: `obj[key] ?? obj[camel(key)]` (null counts as unset).
fn opt<'a>(obj: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    let obj = obj?;
    obj.get(key).filter(|v| !v.is_null()).or_else(|| obj.get(snake_to_camel(key)).filter(|v| !v.is_null()))
}

/// JavaScript truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn opt_string(obj: Option<&Value>, key: &str) -> Option<String> {
    opt(obj, key).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// `String(value)` for values interpolated into messages.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// How a subject token is read from a file or URL response.
#[derive(Debug, Clone, PartialEq)]
pub enum SubjectTokenFormat {
    Text,
    Json { subject_token_field_name: String },
}

/// The source of the external subject token.
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialSource {
    File { path: String, format: SubjectTokenFormat },
    Url { url: String, headers: Option<Map<String, Value>>, format: SubjectTokenFormat },
    Certificate { use_default_certificate_config: bool, certificate_config_location: Option<String>, trust_chain_path: Option<String> },
    Executable { command: String, timeout_millis: u64, output_file: Option<String> },
    Aws {
        region_url: Option<String>,
        security_credentials_url: Option<String>,
        imdsv2_session_token_url: Option<String>,
        regional_cred_verification_url: String,
    },
}

impl CredentialSource {
    fn type_name(&self) -> &'static str {
        match self {
            CredentialSource::File { .. } => "file",
            CredentialSource::Url { .. } => "url",
            CredentialSource::Certificate { .. } => "certificate",
            CredentialSource::Executable { .. } => "executable",
            CredentialSource::Aws { .. } => "aws",
        }
    }
}

/// A parsed `external_account` credential (`BaseExternalAccountClient` and its subclasses).
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalAccountConfig {
    pub audience: String,
    pub subject_token_type: Option<String>,
    pub token_url: String,
    pub service_account_impersonation_url: Option<String>,
    pub service_account_impersonation_lifetime: u64,
    pub config_lifetime_requested: bool,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub workforce_pool_user_project: Option<String>,
    pub quota_project_id: Option<String>,
    pub universe_domain: String,
    pub source: CredentialSource,
}

fn parse_format(source: Option<&Value>) -> anyhow::Result<SubjectTokenFormat> {
    let format = opt(source, "format");
    let format_type = opt_string(format, "type").filter(|t| !t.is_empty()).unwrap_or_else(|| "text".into());
    let field_name = opt_string(format, "subject_token_field_name").filter(|f| !f.is_empty());
    if format_type != "json" && format_type != "text" {
        anyhow::bail!("Invalid credential_source format \"{format_type}\"");
    }
    if format_type == "json" {
        let Some(subject_token_field_name) = field_name else {
            anyhow::bail!("Missing subject_token_field_name for JSON credential_source format");
        };
        return Ok(SubjectTokenFormat::Json { subject_token_field_name });
    }
    Ok(SubjectTokenFormat::Text)
}

impl ExternalAccountConfig {
    /// `ExternalAccountClient.fromJSON` plus the constructor validations, in TS order.
    pub fn from_json(json: &Value) -> anyhow::Result<Self> {
        let options = Some(json);
        let universe_domain = opt_string(options, "universe_domain").unwrap_or_else(|| "googleapis.com".into());
        if let Some(kind) = opt(options, "type").filter(|t| truthy(Some(t)))
            && kind.as_str() != Some("external_account")
        {
            anyhow::bail!("Expected \"external_account\" type but received \"{}\"", js_string(json.get("type")));
        }
        let client_id = opt_string(options, "client_id").filter(|s| !s.is_empty());
        let client_secret = opt_string(options, "client_secret");
        let token_url = opt_string(options, "token_url")
            .unwrap_or_else(|| "https://sts.{universeDomain}/v1/token".replace("{universeDomain}", &universe_domain));
        let subject_token_type = opt_string(options, "subject_token_type");
        let workforce_pool_user_project = opt_string(options, "workforce_pool_user_project").filter(|s| !s.is_empty());
        let service_account_impersonation_url =
            opt_string(options, "service_account_impersonation_url").filter(|s| !s.is_empty());
        let lifetime = opt(opt(options, "service_account_impersonation"), "token_lifetime_seconds")
            .and_then(Value::as_f64)
            .filter(|f| *f != 0.0);
        let Some(audience) = opt_string(options, "audience") else {
            // `this.audience.match(...)` on undefined.
            anyhow::bail!("Cannot read properties of undefined (reading 'match')");
        };
        if workforce_pool_user_project.is_some()
            && !regex::Regex::new(WORKFORCE_AUDIENCE_PATTERN).expect("valid regex").is_match(&audience)
        {
            anyhow::bail!("workforcePoolUserProject should not be set for non-workforce pool credentials.");
        }
        let credential_source = opt(options, "credential_source");
        let source = if truthy(opt(credential_source, "environment_id")) {
            parse_aws_source(credential_source)?
        } else if truthy(credential_source.and_then(|s| s.get("executable"))) {
            parse_executable_source(credential_source)?
        } else {
            parse_identity_pool_source(credential_source)?
        };
        Ok(Self {
            audience,
            subject_token_type,
            token_url,
            service_account_impersonation_url,
            service_account_impersonation_lifetime: lifetime.map(|f| f as u64).unwrap_or(DEFAULT_TOKEN_LIFESPAN),
            config_lifetime_requested: lifetime.is_some(),
            client_id,
            client_secret,
            workforce_pool_user_project,
            quota_project_id: opt_string(options, "quota_project_id"),
            universe_domain,
            source,
        })
    }

    /// The email in `service_account_impersonation_url` (`getServiceAccountEmail`).
    fn service_account_email(&self) -> anyhow::Result<Option<String>> {
        let Some(url) = &self.service_account_impersonation_url else { return Ok(None) };
        if url.chars().count() > 256 {
            anyhow::bail!("URL is too long: {url}");
        }
        let re = regex::Regex::new(r"serviceAccounts/([^:]+):generateAccessToken$").expect("valid regex");
        Ok(re.captures(url).map(|c| c[1].to_string()))
    }

    /// `x-goog-api-client` of the STS exchange (`getMetricsHeaderValue`).
    fn metrics_header_value(&self) -> String {
        format!(
            "gl-node/{NODE_VERSION} auth/{GOOGLE_AUTH_LIBRARY_VERSION} google-byoid-sdk source/{} sa-impersonation/{} config-lifetime/{}",
            self.source.type_name(),
            self.service_account_impersonation_url.is_some(),
            self.config_lifetime_requested
        )
    }
}

fn parse_identity_pool_source(source: Option<&Value>) -> anyhow::Result<CredentialSource> {
    let Some(source) = source.filter(|s| truthy(Some(s))) else {
        anyhow::bail!("A credential source or subject token supplier must be specified.");
    };
    let format = parse_format(Some(source))?;
    let file = opt_string(Some(source), "file").filter(|s| !s.is_empty());
    let url = opt_string(Some(source), "url").filter(|s| !s.is_empty());
    let certificate = opt(Some(source), "certificate").filter(|c| truthy(Some(c)));
    let invalid = "No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.";
    let count = [file.is_some(), url.is_some(), certificate.is_some()].iter().filter(|b| **b).count();
    if count > 1 {
        anyhow::bail!(invalid);
    }
    if let Some(path) = file {
        return Ok(CredentialSource::File { path, format });
    }
    if let Some(url) = url {
        let headers = opt(Some(source), "headers").and_then(Value::as_object).cloned();
        return Ok(CredentialSource::Url { url, headers, format });
    }
    if let Some(certificate) = certificate {
        let use_default = truthy(certificate.get("use_default_certificate_config"));
        let location = certificate.get("certificate_config_location").and_then(Value::as_str).map(str::to_string);
        let has_location = location.as_deref().is_some_and(|l| !l.is_empty());
        if !use_default && !has_location {
            anyhow::bail!(
                "Either `useDefaultCertificateConfig` must be true or a `certificateConfigLocation` must be provided."
            );
        }
        if use_default && has_location {
            anyhow::bail!("Both `useDefaultCertificateConfig` and `certificateConfigLocation` cannot be provided.");
        }
        return Ok(CredentialSource::Certificate {
            use_default_certificate_config: use_default,
            certificate_config_location: location.filter(|l| !l.is_empty()),
            trust_chain_path: certificate.get("trust_chain_path").and_then(Value::as_str).map(str::to_string),
        });
    }
    anyhow::bail!(invalid)
}

fn parse_executable_source(source: Option<&Value>) -> anyhow::Result<CredentialSource> {
    let executable = source.and_then(|s| s.get("executable"));
    let Some(command) = executable.and_then(|e| e.get("command")).filter(|c| truthy(Some(c))).map(|c| js_string(Some(c)))
    else {
        anyhow::bail!("No valid Pluggable Auth \"credential_source\" provided.");
    };
    let timeout_millis = match executable.and_then(|e| e.get("timeout_millis")) {
        None => DEFAULT_EXECUTABLE_TIMEOUT_MILLIS,
        Some(value) => {
            let millis = value.as_f64().unwrap_or(f64::NAN);
            if millis < MINIMUM_EXECUTABLE_TIMEOUT_MILLIS as f64 || millis > MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS as f64 {
                anyhow::bail!(
                    "Timeout must be between {MINIMUM_EXECUTABLE_TIMEOUT_MILLIS} and {MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS} milliseconds."
                );
            }
            millis as u64
        }
    };
    // `PluggableAuthHandler` validates the command eagerly.
    parse_command(&command)?;
    let output_file = executable.and_then(|e| e.get("output_file")).and_then(Value::as_str).map(str::to_string);
    Ok(CredentialSource::Executable { command, timeout_millis, output_file })
}

fn parse_aws_source(source: Option<&Value>) -> anyhow::Result<CredentialSource> {
    let environment_id = opt_string(source, "environment_id").unwrap_or_default();
    let regional_cred_verification_url = opt_string(source, "regional_cred_verification_url").filter(|u| !u.is_empty());
    let version = regex::Regex::new(r"^(aws)(\d+)$").expect("valid regex").captures(&environment_id).map(|c| c[2].to_string());
    let (Some(version), Some(regional_cred_verification_url)) = (version, regional_cred_verification_url) else {
        anyhow::bail!("No valid AWS \"credential_source\" provided");
    };
    if version.parse::<u64>().ok() != Some(1) {
        anyhow::bail!("aws version \"{version}\" is not supported in the current build.");
    }
    Ok(CredentialSource::Aws {
        region_url: opt_string(source, "region_url").filter(|u| !u.is_empty()),
        security_credentials_url: opt_string(source, "url").filter(|u| !u.is_empty()),
        imdsv2_session_token_url: opt_string(source, "imdsv2_session_token_url").filter(|u| !u.is_empty()),
        regional_cred_verification_url,
    })
}

/// `PluggableAuthHandler.parseCommand`.
fn parse_command(command: &str) -> anyhow::Result<Vec<String>> {
    let re = regex::Regex::new(r#"(?:[^\s"]+|"[^"]*")+"#).expect("valid regex");
    let components: Vec<String> = re
        .find_iter(command)
        .map(|m| {
            let part = m.as_str();
            if part.len() >= 2 && part.starts_with('"') && part.ends_with('"') {
                part[1..part.len() - 1].to_string()
            } else {
                part.to_string()
            }
        })
        .collect();
    if components.is_empty() {
        anyhow::bail!("Provided command: \"{command}\" could not be parsed.");
    }
    Ok(components)
}

/// The Gaxios error message of a failed response (`GaxiosError.extractAPIErrorFromResponse`).
pub fn gaxios_error_message(status: u16, body: &str, json_response: bool) -> String {
    let default = format!("Request failed with status code {status}");
    let data: Value = if json_response {
        serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
    } else {
        Value::String(body.to_string())
    };
    let mut message = match &data {
        Value::String(text) => text.clone(),
        _ => default,
    };
    if let Some(error) = data.get("error").filter(|e| truthy(Some(e))) {
        if let Some(text) = error.as_str() {
            return text.to_string();
        }
        if error.is_object() {
            if let Some(text) = error.get("message").and_then(Value::as_str) {
                message = text.to_string();
            }
            if let Some(errors) = error.get("errors").and_then(Value::as_array) {
                let messages: Vec<&str> = errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect();
                if !messages.is_empty() {
                    return messages.join("\n");
                }
            }
        }
    }
    message
}

/// `getErrorFromOAuthErrorResponse`: `Error code <error>[: <description>][ - <uri>]`.
fn oauth_error_message(body: &str, status: u16) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(data) if data.is_object() => {
            let mut message = format!("Error code {}", js_string(data.get("error")));
            if let Some(description) = data.get("error_description") {
                message.push_str(&format!(": {}", js_string(Some(description))));
            }
            if let Some(uri) = data.get("error_uri") {
                message.push_str(&format!(" - {}", js_string(Some(uri))));
            }
            message
        }
        _ => gaxios_error_message(status, body, true),
    }
}

/// `String(err)` of a Node fs error from `realpath`/`lstat` on `path`.
fn node_lstat_error(path: &str) -> String {
    // Node's JS `realpath` lstat()s each prefix and reports the first missing one.
    let mut current = std::path::PathBuf::new();
    for component in Path::new(path).components() {
        current.push(component);
        if std::fs::symlink_metadata(&current).is_err() {
            return format!("ENOENT: no such file or directory, lstat '{}'", current.display());
        }
    }
    format!("ENOENT: no such file or directory, lstat '{path}'")
}

/// `realpath` + `lstat().isFile()` with the auth library's message.
pub(crate) fn resolve_regular_file(path: &str) -> anyhow::Result<std::path::PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => {
            if std::fs::symlink_metadata(&resolved).is_ok_and(|m| m.is_file()) {
                Ok(resolved)
            } else {
                anyhow::bail!("The file at {} does not exist, or it is not a file. ", resolved.display())
            }
        }
        Err(_) => anyhow::bail!("The file at {path} does not exist, or it is not a file. {}", node_lstat_error(path)),
    }
}

fn token_from_json_text(text: &str, field: &str) -> Option<String> {
    let json: Value = serde_json::from_str(text).ok()?;
    let value = json.get(field).filter(|v| truthy(Some(v)))?;
    Some(js_string(Some(value)))
}

async fn file_subject_token(path: &str, format: &SubjectTokenFormat) -> anyhow::Result<String> {
    let resolved = resolve_regular_file(path)?;
    let raw = crate::utils::wasm::fs::read_to_string(&resolved).await?;
    let token = match format {
        SubjectTokenFormat::Text => Some(raw).filter(|t| !t.is_empty()),
        SubjectTokenFormat::Json { subject_token_field_name } => {
            let json: Value = crate::utils::js_json::js_json_parse(&raw).map_err(anyhow::Error::msg)?;
            json.get(subject_token_field_name).filter(|v| truthy(Some(v))).map(|v| js_string(Some(v)))
        }
    };
    token.ok_or_else(|| anyhow::anyhow!("Unable to parse the subject_token from the credential_source file"))
}

fn with_auth_headers(request: reqwest::RequestBuilder, api_client: Option<&str>) -> reqwest::RequestBuilder {
    request
        .header("x-goog-api-client", api_client.map(str::to_string).unwrap_or_else(auth_api_client_header))
        .header("User-Agent", auth_user_agent())
}

/// The Gaxios `retryConfig` of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GaxiosRetry {
    /// Retries for retryable statuses (1xx, 408, 429, 5xx).
    pub retry: u32,
    /// Retries for requests that got no response.
    pub no_response_retries: u32,
}

impl GaxiosRetry {
    /// `AuthClient.RETRY_CONFIG` (and gtoken's `retryConfig`): the Gaxios defaults with POST retried.
    pub(crate) const AUTH: GaxiosRetry = GaxiosRetry { retry: 3, no_response_retries: 2 };
    /// gcp-metadata requests (`retryConfig: { noResponseRetries: 3 }`).
    pub(crate) const METADATA: GaxiosRetry = GaxiosRetry { retry: 3, no_response_retries: 3 };
}

/// Gaxios `getNextRetryDelay`: `retryDelay` (100 ms) on the first retry plus
/// `(2^attempt - 1) / 2` seconds, i.e. 100 ms, 500 ms, 1500 ms.
pub(crate) fn gaxios_retry_delay_ms(attempt: u32) -> u64 {
    let first = if attempt == 0 { 100 } else { 0 };
    first + ((2u64.pow(attempt) - 1) * 1000) / 2
}

/// Send a request the way Gaxios does with retries enabled: a response with a retryable status
/// (1xx, 408, 429, 5xx) is retried up to `retry` times, a request without a response up to
/// `no_response_retries` times, with Gaxios' backoff. The last response (or transport error) is
/// returned.
pub(crate) async fn gaxios_send(
    request: reqwest::RequestBuilder,
    retry: Option<GaxiosRetry>,
) -> reqwest::Result<reqwest::Response> {
    let Some(retry) = retry else { return request.send().await };
    let mut attempt = 0u32;
    loop {
        let Some(this_attempt) = request.try_clone() else { return request.send().await };
        let should_retry = match this_attempt.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let retryable = (100..=199).contains(&status) || status == 408 || status == 429 || (500..=599).contains(&status);
                if !(retryable && attempt < retry.retry) {
                    return Ok(response);
                }
                true
            }
            Err(error) => {
                if attempt >= retry.no_response_retries || attempt >= retry.retry {
                    return Err(error);
                }
                true
            }
        };
        if should_retry {
            crate::utils::wasm::sleep(std::time::Duration::from_millis(gaxios_retry_delay_ms(attempt))).await;
            attempt += 1;
        }
    }
}

async fn send_checked(request: reqwest::RequestBuilder, json_response: bool, retry: Option<GaxiosRetry>) -> anyhow::Result<String> {
    let response = gaxios_send(request, retry).await.map_err(|e| anyhow::anyhow!("request failed, reason: {e}"))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(gaxios_error_message(status, &text, json_response));
    }
    Ok(text)
}

async fn url_subject_token(
    client: &reqwest::Client,
    url: &str,
    headers: Option<&Map<String, Value>>,
    format: &SubjectTokenFormat,
) -> anyhow::Result<String> {
    let mut request = client.get(url);
    if matches!(format, SubjectTokenFormat::Json { .. }) {
        request = request.header("accept", "application/json");
    }
    for (name, value) in headers.into_iter().flatten() {
        request = request.header(name, js_string(Some(value)));
    }
    let request = with_auth_headers(request, None);
    let json_response = matches!(format, SubjectTokenFormat::Json { .. });
    // The identity pool URL supplier sends `RETRY_CONFIG`.
    let text = send_checked(request, json_response, Some(GaxiosRetry::AUTH)).await?;
    let token = match format {
        SubjectTokenFormat::Text => Some(text).filter(|t| !t.is_empty()),
        SubjectTokenFormat::Json { subject_token_field_name } => token_from_json_text(&text, subject_token_field_name),
    };
    token.ok_or_else(|| anyhow::anyhow!("Unable to parse the subject_token from the credential_source URL"))
}

// ---- certificate (X.509) sources ----

fn well_known_certificate_config() -> String {
    let dir = std::env::var("CLOUDSDK_CONFIG").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| {
        if cfg!(windows) {
            Path::new(&std::env::var("APPDATA").unwrap_or_default()).join("gcloud").display().to_string()
        } else {
            Path::new(&std::env::var("HOME").unwrap_or_default()).join(".config").join("gcloud").display().to_string()
        }
    });
    Path::new(&dir).join("certificate_config.json").display().to_string()
}

fn is_valid_file(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn pem_blocks(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"-----BEGIN CERTIFICATE-----[^-]+-----END CERTIFICATE-----").expect("valid regex");
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .flat_map(|l| l.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = STANDARD.decode(body).map_err(|e| anyhow::anyhow!("error:0680007B:asn1 encoding routines::header too long ({e})"))?;
    if der.first() != Some(&0x30) {
        anyhow::bail!("error:0680007B:asn1 encoding routines::header too long");
    }
    Ok(der)
}

/// The mTLS identity and subject token of a certificate source.
struct CertificateMaterial {
    identity_pem: Vec<u8>,
    subject_token: String,
}

async fn certificate_material(
    use_default_certificate_config: bool,
    certificate_config_location: Option<&str>,
    trust_chain_path: Option<&str>,
) -> anyhow::Result<CertificateMaterial> {
    let config_path = match certificate_config_location.filter(|_| !use_default_certificate_config) {
        Some(path) => {
            if !is_valid_file(path) {
                anyhow::bail!("Provided certificate config path is invalid: {path}");
            }
            path.to_string()
        }
        None => match std::env::var(CERTIFICATE_CONFIGURATION_ENV_VARIABLE).ok().filter(|v| !v.is_empty()) {
            Some(path) => {
                if !is_valid_file(&path) {
                    anyhow::bail!("Path from environment variable \"{CERTIFICATE_CONFIGURATION_ENV_VARIABLE}\" is invalid: {path}");
                }
                path
            }
            None => {
                let well_known = well_known_certificate_config();
                if !is_valid_file(&well_known) {
                    anyhow::bail!(
                        "Could not find certificate configuration file. Searched override path, the \"{CERTIFICATE_CONFIGURATION_ENV_VARIABLE}\" env var, and the gcloud path ({well_known})."
                    );
                }
                well_known
            }
        },
    };
    let contents = crate::utils::wasm::fs::read_to_string(&config_path)
        .await
        .map_err(|_| anyhow::anyhow!("Failed to read certificate config file at: {config_path}"))?;
    let config: Value = crate::utils::js_json::js_json_parse(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate config from {config_path}: {e}"))?;
    let workload = config.get("cert_configs").and_then(|c| c.get("workload"));
    let cert_path = workload.and_then(|w| w.get("cert_path")).and_then(Value::as_str).filter(|p| !p.is_empty());
    let key_path = workload.and_then(|w| w.get("key_path")).and_then(Value::as_str).filter(|p| !p.is_empty());
    let (Some(cert_path), Some(key_path)) = (cert_path, key_path) else {
        anyhow::bail!(
            "Certificate config file ({config_path}) is missing required \"cert_path\" or \"key_path\" in the workload config."
        );
    };
    let cert = crate::utils::wasm::fs::read(cert_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read certificate file at {cert_path}: {e}"))?;
    let cert_text = String::from_utf8_lossy(&cert).into_owned();
    let leaf_pem = pem_blocks(&cert_text)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to read certificate file at {cert_path}: error:0480006C:PEM routines::no start line"))?;
    let leaf = pem_to_der(&leaf_pem).map_err(|e| anyhow::anyhow!("Failed to read certificate file at {cert_path}: {e}"))?;
    let key = crate::utils::wasm::fs::read(key_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read private key file at {key_path}: {e}"))?;
    let mut identity_pem = cert.clone();
    identity_pem.push(b'\n');
    identity_pem.extend_from_slice(&key);
    #[cfg(not(target_arch = "wasm32"))]
    reqwest::Identity::from_pem(&identity_pem)
        .map_err(|e| anyhow::anyhow!("Failed to read private key file at {key_path}: {e}"))?;

    let chain: Vec<Vec<u8>> = match trust_chain_path.filter(|p| !p.is_empty()) {
        None => vec![leaf],
        Some(path) => {
            let pems = crate::utils::wasm::fs::read_to_string(path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to process certificate chain from {path}: {e}"))?;
            let mut certs = Vec::new();
            for (index, pem) in pem_blocks(&pems).iter().enumerate() {
                certs.push(pem_to_der(pem).map_err(|e| {
                    anyhow::anyhow!("Failed to parse certificate at index {index} in trust chain file {path}: {e}")
                })?);
            }
            match certs.iter().position(|c| *c == leaf) {
                None => std::iter::once(leaf).chain(certs).collect(),
                Some(0) => certs,
                Some(index) => anyhow::bail!(
                    "Leaf certificate exists in the trust chain but is not the first entry (found at index {index})."
                ),
            }
        }
    };
    let encoded: Vec<String> = chain.iter().map(|der| STANDARD.encode(der)).collect();
    Ok(CertificateMaterial { identity_pem, subject_token: serde_json::to_string(&encoded)? })
}

/// wasm32: the host's `fetch` cannot present client certificates.
#[cfg(target_arch = "wasm32")]
fn mtls_client(_identity_pem: &[u8]) -> anyhow::Result<reqwest::Client> {
    anyhow::bail!("Certificate-based external accounts are not available on wasm32")
}
#[cfg(not(target_arch = "wasm32"))]
fn mtls_client(identity_pem: &[u8]) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().use_rustls_tls().identity(reqwest::Identity::from_pem(identity_pem)?).build()?)
}

// ---- executable sources ----

#[derive(Debug)]
struct ExecutableResponse {
    version: i64,
    success: bool,
    expiration_time: Option<f64>,
    error_code: String,
    error_message: String,
    subject_token: String,
}

fn now_seconds_rounded() -> f64 {
    (crate::utils::now_ms() as f64 / 1000.0).round()
}

impl ExecutableResponse {
    fn parse(json: &Value) -> anyhow::Result<Self> {
        if !truthy(json.get("version")) {
            anyhow::bail!("Executable response must contain a 'version' field.");
        }
        let Some(success) = json.get("success") else {
            anyhow::bail!("Executable response must contain a 'success' field.");
        };
        let mut response = ExecutableResponse {
            version: json["version"].as_f64().map(|v| v as i64).unwrap_or(0),
            success: truthy(Some(success)),
            expiration_time: None,
            error_code: String::new(),
            error_message: String::new(),
            subject_token: String::new(),
        };
        if response.success {
            response.expiration_time = json.get("expiration_time").and_then(Value::as_f64);
            let token_type = json.get("token_type").and_then(Value::as_str).unwrap_or_default();
            if ![SAML_SUBJECT_TOKEN_TYPE, OIDC_SUBJECT_TOKEN_TYPE1, OIDC_SUBJECT_TOKEN_TYPE2].contains(&token_type) {
                anyhow::bail!(
                    "Executable response must contain a 'token_type' field when successful and it must be one of {OIDC_SUBJECT_TOKEN_TYPE1}, {OIDC_SUBJECT_TOKEN_TYPE2}, or {SAML_SUBJECT_TOKEN_TYPE}."
                );
            }
            if token_type == SAML_SUBJECT_TOKEN_TYPE {
                if !truthy(json.get("saml_response")) {
                    anyhow::bail!(
                        "Executable response must contain a 'saml_response' field when token_type={SAML_SUBJECT_TOKEN_TYPE}."
                    );
                }
                response.subject_token = js_string(json.get("saml_response"));
            } else {
                if !truthy(json.get("id_token")) {
                    anyhow::bail!(
                        "Executable response must contain a 'id_token' field when token_type={OIDC_SUBJECT_TOKEN_TYPE1} or {OIDC_SUBJECT_TOKEN_TYPE2}."
                    );
                }
                response.subject_token = js_string(json.get("id_token"));
            }
        } else {
            if !truthy(json.get("code")) {
                anyhow::bail!("Executable response must contain a 'code' field when unsuccessful.");
            }
            if !truthy(json.get("message")) {
                anyhow::bail!("Executable response must contain a 'message' field when unsuccessful.");
            }
            response.error_code = js_string(json.get("code"));
            response.error_message = js_string(json.get("message"));
        }
        Ok(response)
    }

    fn is_expired(&self) -> bool {
        self.expiration_time.is_some_and(|t| t < now_seconds_rounded())
    }
}

fn executable_error(message: &str, code: &str) -> anyhow::Error {
    anyhow::anyhow!("The executable failed with exit code: {code} and error message: {message}.")
}

async fn cached_executable_response(output_file: &str) -> anyhow::Result<Option<ExecutableResponse>> {
    if output_file.is_empty() {
        return Ok(None);
    }
    let Ok(path) = std::fs::canonicalize(output_file) else { return Ok(None) };
    if !std::fs::symlink_metadata(&path)?.is_file() {
        return Ok(None);
    }
    let text = crate::utils::wasm::fs::read_to_string(&path).await?;
    if text.is_empty() {
        return Ok(None);
    }
    let json: Value = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("The output file contained an invalid response: {text}"))?;
    let response = ExecutableResponse::parse(&json)?;
    Ok((!response.is_expired() && response.success).then_some(response))
}

/// wasm32 cannot run processes.
#[cfg(target_arch = "wasm32")]
async fn run_executable(_command: &str, _timeout_millis: u64, _env: &[(String, String)]) -> anyhow::Result<ExecutableResponse> {
    anyhow::bail!("Executable-sourced external accounts are not available on wasm32")
}
#[cfg(not(target_arch = "wasm32"))]
async fn run_executable(command: &str, timeout_millis: u64, env: &[(String, String)]) -> anyhow::Result<ExecutableResponse> {
    use tokio::io::AsyncReadExt;
    let parts = parse_command(command)?;
    let mut child = tokio::process::Command::new(&parts[0])
        .args(&parts[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let run = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a?;
        b?;
        let status = child.wait().await?;
        // TS appends stdout and stderr chunks as they arrive; stdout then stderr here.
        out.extend_from_slice(&err);
        anyhow::Ok((status, String::from_utf8_lossy(&out).into_owned()))
    };
    let (status, output) = match crate::utils::wasm::timeout(Duration::from_millis(timeout_millis), run).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("The executable failed to finish within the timeout specified."),
    };
    if status.code() == Some(0) {
        let json: Value = serde_json::from_str(&output)
            .map_err(|_| anyhow::anyhow!("The executable returned an invalid response: {output}"))?;
        return ExecutableResponse::parse(&json);
    }
    let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "null".into());
    Err(executable_error(&output, &code))
}

async fn executable_subject_token(
    config: &ExternalAccountConfig,
    command: &str,
    timeout_millis: u64,
    output_file: Option<&str>,
) -> anyhow::Result<String> {
    if std::env::var("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").ok().as_deref() != Some("1") {
        anyhow::bail!(
            "Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1."
        );
    }
    let output_file = output_file.filter(|f| !f.is_empty());
    let mut response = match output_file {
        Some(file) => cached_executable_response(file).await?,
        None => None,
    };
    if response.is_none() {
        let mut env = vec![
            ("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE".to_string(), config.audience.clone()),
            ("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE".to_string(), config.subject_token_type.clone().unwrap_or_default()),
            ("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE".to_string(), "0".to_string()),
        ];
        if let Some(file) = output_file {
            env.push(("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE".to_string(), file.to_string()));
        }
        if let Some(email) = config.service_account_email()? {
            env.push(("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL".to_string(), email));
        }
        response = Some(run_executable(command, timeout_millis, &env).await?);
    }
    let response = response.expect("set above");
    if response.version > MAXIMUM_EXECUTABLE_VERSION {
        anyhow::bail!(
            "Version of executable is not currently supported, maximum supported version is {MAXIMUM_EXECUTABLE_VERSION}."
        );
    }
    if !response.success {
        return Err(executable_error(&response.error_message, &response.error_code));
    }
    if output_file.is_some() && response.expiration_time.is_none() {
        anyhow::bail!(
            "The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration."
        );
    }
    if response.is_expired() {
        anyhow::bail!("Executable response is expired.");
    }
    Ok(response.subject_token)
}

// ---- AWS sources ----

struct AwsSecurityCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: Option<String>,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn imds_session_token(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let request = with_auth_headers(client.put(url).header("x-aws-ec2-metadata-token-ttl-seconds", "300"), None);
    send_checked(request, false, None).await
}

async fn aws_region(client: &reqwest::Client, region_url: Option<&str>, imds_url: Option<&str>) -> anyhow::Result<String> {
    if let Some(region) = env_nonempty("AWS_REGION").or_else(|| env_nonempty("AWS_DEFAULT_REGION")) {
        return Ok(region);
    }
    let token = match imds_url {
        Some(url) => Some(imds_session_token(client, url).await?),
        None => None,
    };
    let Some(region_url) = region_url else {
        anyhow::bail!("Unable to determine AWS region due to missing \"options.credential_source.region_url\"");
    };
    let mut request = client.get(region_url);
    if let Some(token) = &token {
        request = request.header("x-aws-ec2-metadata-token", token);
    }
    // The AWS supplier is built without `additionalGaxiosOptions`: no retries.
    let zone = send_checked(with_auth_headers(request, None), false, None).await?;
    // The availability zone minus its trailing letter.
    let mut chars: Vec<char> = zone.chars().collect();
    chars.pop();
    Ok(chars.into_iter().collect())
}

async fn aws_security_credentials(
    client: &reqwest::Client,
    url: Option<&str>,
    imds_url: Option<&str>,
) -> anyhow::Result<AwsSecurityCredentials> {
    if let (Some(access_key_id), Some(secret_access_key)) =
        (env_nonempty("AWS_ACCESS_KEY_ID"), env_nonempty("AWS_SECRET_ACCESS_KEY"))
    {
        return Ok(AwsSecurityCredentials { access_key_id, secret_access_key, token: env_nonempty("AWS_SESSION_TOKEN") });
    }
    let token = match imds_url {
        Some(url) => Some(imds_session_token(client, url).await?),
        None => None,
    };
    let Some(url) = url else {
        anyhow::bail!("Unable to determine AWS role name due to missing \"options.credential_source.url\"");
    };
    let with_token = |request: reqwest::RequestBuilder| match &token {
        Some(token) => request.header("x-aws-ec2-metadata-token", token),
        None => request,
    };
    let role = send_checked(with_auth_headers(with_token(client.get(url)), None), false, None).await?;
    let text = send_checked(with_auth_headers(with_token(client.get(format!("{url}/{role}"))), None), true, None).await?;
    let data: Value = crate::utils::js_json::js_json_parse(&text).map_err(anyhow::Error::msg)?;
    Ok(AwsSecurityCredentials {
        access_key_id: js_string(data.get("AccessKeyId")),
        secret_access_key: js_string(data.get("SecretAccessKey")),
        token: data.get("Token").and_then(Value::as_str).map(str::to_string),
    })
}

fn hmac_sha256(key: &[u8], message: &str) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &str) -> String {
    use sha2::Digest;
    super::aws_sigv4::hex(&sha2::Sha256::digest(data.as_bytes()))
}

/// The serialized, signed `GetCallerIdentity` request (`AwsClient.retrieveSubjectToken`).
fn aws_subject_token(
    verification_url: &str,
    region: &str,
    credentials: &AwsSecurityCredentials,
    audience: &str,
    now_ms: i64,
) -> anyhow::Result<String> {
    let url = verification_url.replace("{region}", region);
    let parsed = url::Url::parse(&url)?;
    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    let canonical_uri = parsed.path().to_string();
    let canonical_query = parsed.query().unwrap_or_default().to_string();
    let service = host.split('.').next().unwrap_or_default().to_string();
    let amz_date = super::aws_sigv4::amz_date(now_ms);
    let date_stamp = &amz_date[..8];
    // Signed headers, sorted: host, x-amz-date, x-amz-security-token.
    let mut signed: Vec<(&str, String)> = vec![("host", host.clone()), ("x-amz-date", amz_date.clone())];
    if let Some(token) = &credentials.token {
        signed.push(("x-amz-security-token", token.clone()));
    }
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = signed.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical_request =
        format!("POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{}", sha256_hex(""));
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(&canonical_request));
    let k_date = hmac_sha256(format!("AWS4{}", credentials.secret_access_key).as_bytes(), date_stamp);
    let k_region = hmac_sha256(&k_date, region);
    let k_service = hmac_sha256(&k_region, &service);
    let k_signing = hmac_sha256(&k_service, "aws4_request");
    let signature = super::aws_sigv4::hex(&hmac_sha256(&k_signing, &string_to_sign));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    // `Headers` iterate in sorted order.
    let mut headers = vec![
        ("authorization", authorization),
        ("host", host),
        ("x-amz-date", amz_date),
    ];
    if let Some(token) = &credentials.token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    headers.push(("x-goog-cloud-target-resource", audience.to_string()));
    let request = json!({
        "url": url,
        "method": "POST",
        "headers": headers.iter().map(|(key, value)| json!({ "key": key, "value": value })).collect::<Vec<_>>(),
    });
    Ok(encode_uri_component(&request.to_string()))
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---- token exchange ----

/// An access token and its expiry in epoch milliseconds.
pub type AccessToken = (String, Option<i64>);

/// The subject token plus, for certificate sources, the mTLS client used for STS and impersonation.
async fn retrieve_subject_token(
    config: &ExternalAccountConfig,
    client: &reqwest::Client,
) -> anyhow::Result<(String, Option<reqwest::Client>)> {
    match &config.source {
        CredentialSource::File { path, format } => Ok((file_subject_token(path, format).await?, None)),
        CredentialSource::Url { url, headers, format } => {
            Ok((url_subject_token(client, url, headers.as_ref(), format).await?, None))
        }
        CredentialSource::Certificate { use_default_certificate_config, certificate_config_location, trust_chain_path } => {
            let material = certificate_material(
                *use_default_certificate_config,
                certificate_config_location.as_deref(),
                trust_chain_path.as_deref(),
            )
            .await?;
            Ok((material.subject_token, Some(mtls_client(&material.identity_pem)?)))
        }
        CredentialSource::Executable { command, timeout_millis, output_file } => {
            Ok((executable_subject_token(config, command, *timeout_millis, output_file.as_deref()).await?, None))
        }
        CredentialSource::Aws {
            region_url,
            security_credentials_url,
            imdsv2_session_token_url,
            regional_cred_verification_url,
        } => {
            let region = aws_region(client, region_url.as_deref(), imdsv2_session_token_url.as_deref()).await?;
            let credentials =
                aws_security_credentials(client, security_credentials_url.as_deref(), imdsv2_session_token_url.as_deref())
                    .await?;
            let token =
                aws_subject_token(regional_cred_verification_url, &region, &credentials, &config.audience, crate::utils::now_ms())?;
            Ok((token, None))
        }
    }
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish()
}

fn basic_auth(client_id: &str, client_secret: Option<&str>) -> String {
    format!("Basic {}", STANDARD.encode(format!("{client_id}:{}", client_secret.unwrap_or_default())))
}

fn expiry_from_expires_in(data: &Value) -> Option<i64> {
    data.get("expires_in")
        .and_then(Value::as_f64)
        .filter(|s| *s != 0.0)
        .map(|s| crate::utils::now_ms() + (s * 1000.0) as i64)
}

/// `BaseExternalAccountClient.refreshAccessTokenAsync`: subject token, STS exchange, then
/// optional service account impersonation.
pub async fn external_account_access_token(
    config: &ExternalAccountConfig,
    scope: &str,
    client: &reqwest::Client,
) -> anyhow::Result<AccessToken> {
    let (subject_token, mtls) = retrieve_subject_token(config, client).await?;
    let client = mtls.as_ref().unwrap_or(client);
    let scope_value = if config.service_account_impersonation_url.is_some() { DEFAULT_OAUTH_SCOPE } else { scope };
    let options = match (&config.client_id, &config.workforce_pool_user_project) {
        (None, Some(project)) => Some(json!({ "userProject": project }).to_string()),
        _ => None,
    };
    let mut pairs: Vec<(&str, &str)> = vec![
        ("grant_type", STS_GRANT_TYPE),
        ("audience", &config.audience),
        ("scope", scope_value),
        ("requested_token_type", STS_REQUEST_TOKEN_TYPE),
        ("subject_token", &subject_token),
    ];
    if let Some(kind) = &config.subject_token_type {
        pairs.push(("subject_token_type", kind));
    }
    if let Some(options) = &options {
        pairs.push(("options", options));
    }
    let mut request = client
        .post(&config.token_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded;charset=UTF-8")
        .body(form_body(&pairs));
    if let Some(client_id) = &config.client_id {
        request = request.header("authorization", basic_auth(client_id, config.client_secret.as_deref()));
    }
    let request = with_auth_headers(request, Some(&config.metrics_header_value()));
    let response = gaxios_send(request, Some(GaxiosRetry::AUTH))
        .await
        .map_err(|e| anyhow::anyhow!("request to {} failed, reason: {e}", config.token_url))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(oauth_error_message(&text, status));
    }
    let sts: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let sts_token = js_string(sts.get("access_token"));

    let Some(impersonation_url) = &config.service_account_impersonation_url else {
        return Ok((sts_token, expiry_from_expires_in(&sts)));
    };
    let request = client
        .post(impersonation_url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {sts_token}"))
        .body(
            json!({
                "scope": [scope],
                "lifetime": format!("{}s", config.service_account_impersonation_lifetime),
            })
            .to_string(),
        );
    let text = send_checked(with_auth_headers(request, None), true, Some(GaxiosRetry::AUTH)).await?;
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let token = js_string(data.get("accessToken"));
    let expires_at = data.get("expireTime").and_then(Value::as_str).and_then(super::google_auth::parse_rfc3339_ms);
    Ok((token, expires_at))
}

/// A parsed `external_account_authorized_user` credential.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalAccountAuthorizedUserConfig {
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token_url: String,
    pub quota_project_id: Option<String>,
}

impl ExternalAccountAuthorizedUserConfig {
    pub fn from_json(json: &Value) -> Self {
        let universe = json
            .get("universe_domain")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| opt_string(Some(json), "universe_domain"))
            .unwrap_or_else(|| "googleapis.com".into());
        Self {
            refresh_token: json.get("refresh_token").map(|v| js_string(Some(v))),
            client_id: json.get("client_id").map(|v| js_string(Some(v))),
            client_secret: json.get("client_secret").map(|v| js_string(Some(v))),
            token_url: json
                .get("token_url")
                .filter(|v| !v.is_null())
                .map(|v| js_string(Some(v)))
                .unwrap_or_else(|| format!("https://sts.{universe}/v1/oauthtoken")),
            quota_project_id: opt_string(Some(json), "quota_project_id"),
        }
    }
}

/// `ExternalAccountAuthorizedUserClient.refreshAccessTokenAsync`.
pub async fn external_account_authorized_user_token(
    config: &ExternalAccountAuthorizedUserConfig,
    client: &reqwest::Client,
) -> anyhow::Result<AccessToken> {
    let refresh_token = config.refresh_token.clone().unwrap_or_else(|| "undefined".into());
    let request = client
        .post(&config.token_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded;charset=UTF-8")
        .header(
            "authorization",
            basic_auth(config.client_id.as_deref().unwrap_or("undefined"), config.client_secret.as_deref()),
        )
        .body(form_body(&[("grant_type", "refresh_token"), ("refresh_token", &refresh_token)]));
    let response = gaxios_send(with_auth_headers(request, None), Some(GaxiosRetry::AUTH))
        .await
        .map_err(|e| anyhow::anyhow!("request to {} failed, reason: {e}", config.token_url))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        anyhow::bail!(oauth_error_message(&text, status));
    }
    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let expires_in = data.get("expires_in").and_then(Value::as_f64).unwrap_or(f64::NAN);
    // `new Date().getTime() + expires_in * 1000`: NaN (never expires) without `expires_in`.
    let expires_at = expires_in.is_finite().then(|| crate::utils::now_ms() + (expires_in * 1000.0) as i64);
    Ok((js_string(data.get("access_token")), expires_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_aliases() {
        assert_eq!(snake_to_camel("token_url"), "tokenUrl");
        assert_eq!(snake_to_camel("service_account_impersonation_url"), "serviceAccountImpersonationUrl");
        let json = json!({ "tokenUrl": "x" });
        assert_eq!(opt_string(Some(&json), "token_url").as_deref(), Some("x"));
    }

    #[test]
    fn parses_commands_like_the_auth_library() {
        assert_eq!(parse_command(r#"/bin/run "a b" c"#).unwrap(), ["/bin/run", "a b", "c"]);
        assert!(parse_command("   ").is_err());
    }

    #[test]
    fn gaxios_messages() {
        // An empty body is the (string) response data, so it is the message.
        assert_eq!(gaxios_error_message(404, "", true), "");
        assert_eq!(gaxios_error_message(404, "[]", true), "Request failed with status code 404");
        assert_eq!(gaxios_error_message(400, r#"{"error":"bad"}"#, true), "bad");
        assert_eq!(gaxios_error_message(403, r#"{"error":{"message":"denied"}}"#, true), "denied");
        assert_eq!(gaxios_error_message(500, "oops", false), "oops");
        assert_eq!(
            oauth_error_message(r#"{"error":"invalid_grant","error_description":"expired"}"#, 400),
            "Error code invalid_grant: expired"
        );
    }

    #[test]
    fn gaxios_backoff() {
        assert_eq!((0..3).map(gaxios_retry_delay_ms).collect::<Vec<_>>(), vec![100, 500, 1500]);
    }

    #[test]
    fn encodes_uri_components() {
        assert_eq!(encode_uri_component("{\"a\":\"b c/d\"}"), "%7B%22a%22%3A%22b%20c%2Fd%22%7D");
    }
}
