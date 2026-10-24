//! OAuth metadata, token, and client types with dependency-free structural validation.
//! Adapted from modelcontextprotocol/typescript-sdk v1.29.0 (MIT, Copyright (c) 2024 Anthropic, PBC).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    pub response_types_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AuthorizationServerMetadata {
    /// Metadata with the required fields only.
    pub fn new(
        issuer: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            authorization_endpoint: authorization_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            registration_endpoint: None,
            scopes_supported: None,
            response_types_supported: vec!["code".into()],
            grant_types_supported: None,
            token_endpoint_auth_methods_supported: None,
            code_challenge_methods_supported: None,
            client_id_metadata_document_supported: None,
            authorization_response_iss_parameter_supported: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_util::opt_number"
    )]
    pub expires_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

impl OAuthTokens {
    pub fn bearer(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            token_type: "Bearer".into(),
            expires_in: None,
            scope: None,
            refresh_token: None,
            id_token: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientMetadata {
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    /// OpenID Connect client type, `native` or `web`. Dynamic client registration derives it from
    /// `redirect_uris` when absent (MCP SEP-837).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contacts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tos_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_statement: Option<String>,
}

/// Client credentials, optionally with the registered client metadata (`OAuthClientInformation`,
/// `OAuthClientInformationFull` and `OAuthClientInformationMixed` in TypeScript). Metadata fields
/// such as `redirect_uris` live in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientInformation {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_util::opt_number"
    )]
    pub client_id_issued_at: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_util::opt_number"
    )]
    pub client_secret_expires_at: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub type OAuthClientInformationFull = OAuthClientInformation;
pub type OAuthClientInformationMixed = OAuthClientInformation;

impl OAuthClientInformation {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            client_id_issued_at: None,
            client_secret_expires_at: None,
            extra: Map::new(),
        }
    }

    pub fn with_secret(mut self, client_secret: impl Into<String>) -> Self {
        self.client_secret = Some(client_secret.into());
        self
    }

    /// Whether the information includes registered metadata (`"redirect_uris" in information`).
    pub fn has_redirect_uris(&self) -> bool {
        self.extra.contains_key("redirect_uris")
    }

    /// Registered redirect URIs; empty without registered metadata.
    pub fn redirect_uris(&self) -> Vec<String> {
        self.extra
            .get("redirect_uris")
            .and_then(Value::as_array)
            .map(|uris| {
                uris.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The registered `token_endpoint_auth_method`, if any.
    pub fn token_endpoint_auth_method(&self) -> Option<&str> {
        self.extra
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthDiscoveryState {
    pub authorization_server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OAuthServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    pub resource_metadata: Option<OAuthProtectedResourceMetadata>,
}

/// A parsed `WWW-Authenticate` bearer challenge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthChallenge {
    pub resource_metadata_url: Option<Url>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn invalid(name: &str) -> Error {
    Error::Other(format!("Invalid {name}"))
}

fn object(value: Value, name: &str) -> Result<Map<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(invalid(name)),
    }
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(invalid(name)),
    }
}

/// Treats `null` and `""` as absent: servers send them for fields they have no value for, like `scope: ""`.
fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null)) || value.and_then(Value::as_str) == Some("")
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>> {
    if absent(value) {
        return Ok(None);
    }
    required_string(value, name).map(Some)
}

fn optional_strings(value: Option<&Value>, name: &str) -> Result<Option<Vec<String>>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid(name))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(_) => Err(invalid(name)),
    }
}

/// Only HTTP(S) URLs are safe for server-supplied OAuth metadata and the platform URL handler.
pub fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

fn safe_url(value: Option<&Value>, name: &str) -> Result<String> {
    let text = required_string(value, name)?;
    // URL parsing errors are reported as plain errors here; discovery reserves `TypeError` for network failures.
    let url = Url::parse(&text).map_err(|_| invalid(name))?;
    if !is_http_url(&url) {
        return Err(invalid(name));
    }
    Ok(text)
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<String>> {
    if absent(value) {
        return Ok(None);
    }
    safe_url(value, name).map(Some)
}

fn set_or_remove(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            object.insert(key.to_string(), value);
        }
        None => {
            object.remove(key);
        }
    }
}

fn strings_value(values: Option<Vec<String>>) -> Option<Value> {
    values.map(|values| Value::Array(values.into_iter().map(Value::String).collect()))
}

fn from_object<T: serde::de::DeserializeOwned>(
    object: Map<String, Value>,
    name: &str,
) -> Result<T> {
    serde_json::from_value(Value::Object(object)).map_err(|_| invalid(name))
}

pub fn parse_protected_resource_metadata(value: Value) -> Result<OAuthProtectedResourceMetadata> {
    let mut input = object(value, "OAuth protected resource metadata")?;
    let resource = safe_url(
        input.get("resource"),
        "OAuth protected resource metadata resource",
    )?;
    let servers = optional_strings(input.get("authorization_servers"), "authorization_servers")?
        .map(|servers| {
            servers
                .into_iter()
                .map(|url| safe_url(Some(&Value::String(url)), "authorization server URL"))
                .collect()
        })
        .transpose()?;
    let scopes = optional_strings(input.get("scopes_supported"), "scopes_supported")?;
    input.insert("resource".into(), Value::String(resource));
    set_or_remove(&mut input, "authorization_servers", strings_value(servers));
    set_or_remove(&mut input, "scopes_supported", strings_value(scopes));
    from_object(input, "OAuth protected resource metadata")
}

pub fn parse_authorization_server_metadata(value: Value) -> Result<AuthorizationServerMetadata> {
    let mut input = object(value, "authorization server metadata")?;
    let response_types = optional_strings(
        input.get("response_types_supported"),
        "response_types_supported",
    )?
    .ok_or_else(|| invalid("response_types_supported"))?;
    let issuer = safe_url(input.get("issuer"), "authorization server issuer")?;
    let authorization_endpoint = safe_url(
        input.get("authorization_endpoint"),
        "authorization endpoint",
    )?;
    let token_endpoint = safe_url(input.get("token_endpoint"), "token endpoint")?;
    let registration_endpoint =
        optional_url(input.get("registration_endpoint"), "registration endpoint")?;
    let scopes = optional_strings(input.get("scopes_supported"), "scopes_supported")?;
    let grant_types =
        optional_strings(input.get("grant_types_supported"), "grant_types_supported")?;
    let auth_methods = optional_strings(
        input.get("token_endpoint_auth_methods_supported"),
        "token_endpoint_auth_methods_supported",
    )?;
    let challenge_methods = optional_strings(
        input.get("code_challenge_methods_supported"),
        "code_challenge_methods_supported",
    )?;
    let boolean = |value: Option<&Value>| value.and_then(Value::as_bool).map(Value::Bool);
    let cimd = boolean(input.get("client_id_metadata_document_supported"));
    let iss = boolean(input.get("authorization_response_iss_parameter_supported"));
    input.insert("issuer".into(), Value::String(issuer));
    input.insert(
        "authorization_endpoint".into(),
        Value::String(authorization_endpoint),
    );
    input.insert("token_endpoint".into(), Value::String(token_endpoint));
    set_or_remove(
        &mut input,
        "registration_endpoint",
        registration_endpoint.map(Value::String),
    );
    set_or_remove(&mut input, "scopes_supported", strings_value(scopes));
    input.insert(
        "response_types_supported".into(),
        strings_value(Some(response_types)).unwrap_or(Value::Null),
    );
    set_or_remove(
        &mut input,
        "grant_types_supported",
        strings_value(grant_types),
    );
    set_or_remove(
        &mut input,
        "token_endpoint_auth_methods_supported",
        strings_value(auth_methods),
    );
    set_or_remove(
        &mut input,
        "code_challenge_methods_supported",
        strings_value(challenge_methods),
    );
    set_or_remove(&mut input, "client_id_metadata_document_supported", cimd);
    set_or_remove(
        &mut input,
        "authorization_response_iss_parameter_supported",
        iss,
    );
    from_object(input, "authorization server metadata")
}

/// `Number(value)` for a JSON value: primitives convert directly, arrays and objects through
/// their string form (`[5]` is `"5"`, `[]` and `[null]` are `""`, objects are `"[object Object]"`).
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::Bool(value) => f64::from(u8::from(*value)),
        Value::Null => 0.0,
        Value::String(text) => js_string_to_number(text),
        Value::Array(_) | Value::Object(_) => js_string_to_number(&js_to_string(value)),
    }
}

/// `String(value)` for a JSON value (`Array.prototype.join` for arrays).
fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => match number.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{f}"),
            _ => number.to_string(),
        },
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_to_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// ECMAScript `StringToNumber`: surrounding JS whitespace is ignored, an empty string is 0,
/// `0x`/`0o`/`0b` prefixes select a radix (unsigned only), `Infinity` may be signed, anything
/// else must be a decimal literal; otherwise `NaN`.
fn js_string_to_number(text: &str) -> f64 {
    let is_js_space = |c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
                | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
        )
    };
    let text = text.trim_matches(is_js_space);
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            return digits.chars().try_fold(0.0f64, |acc, c| c.to_digit(radix).map(|d| acc * f64::from(radix) + f64::from(d))).unwrap_or(f64::NAN);
        }
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return if text.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    // StrUnsignedDecimalLiteral: digits with an optional fraction (at least one digit in all),
    // then an optional exponent. Rust's parser accepts more (`inf`, `nan`), so check first.
    let bytes = unsigned.as_bytes();
    let mut i = 0;
    let int_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    i += int_digits;
    let mut frac_digits = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        i += frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return f64::NAN;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        if exp_digits == 0 {
            return f64::NAN;
        }
        i += exp_digits;
    }
    if i != bytes.len() {
        return f64::NAN;
    }
    text.parse().unwrap_or(f64::NAN)
}

pub fn parse_oauth_tokens(value: Option<Value>) -> Result<OAuthTokens> {
    let input = object(value.unwrap_or(Value::Null), "OAuth token response")?;
    // `Number(null)` is 0, which would mark the token as expired at once.
    let expires = if absent(input.get("expires_in")) {
        None
    } else {
        input.get("expires_in").map(js_number)
    };
    if expires.is_some_and(|expires| !expires.is_finite()) {
        return Err(invalid("expires_in"));
    }
    Ok(OAuthTokens {
        access_token: required_string(input.get("access_token"), "access_token")?,
        token_type: required_string(input.get("token_type"), "token_type")?,
        expires_in: expires,
        scope: optional_string(input.get("scope"), "scope")?,
        refresh_token: optional_string(input.get("refresh_token"), "refresh_token")?,
        id_token: optional_string(input.get("id_token"), "id_token")?,
    })
}

pub fn parse_client_information(value: Value) -> Result<OAuthClientInformationFull> {
    let mut input = object(value, "OAuth client registration response")?;
    let client_id = required_string(input.get("client_id"), "client_id")?;
    let client_secret = optional_string(input.get("client_secret"), "client_secret")?;
    let number = |value: Option<&Value>| value.filter(|value| value.is_number()).cloned();
    let issued_at = number(input.get("client_id_issued_at"));
    let expires_at = number(input.get("client_secret_expires_at"));
    let redirect_uris =
        optional_strings(input.get("redirect_uris"), "redirect_uris")?.unwrap_or_default();
    input.insert("client_id".into(), Value::String(client_id));
    set_or_remove(
        &mut input,
        "client_secret",
        client_secret.map(Value::String),
    );
    set_or_remove(&mut input, "client_id_issued_at", issued_at);
    set_or_remove(&mut input, "client_secret_expires_at", expires_at);
    input.insert(
        "redirect_uris".into(),
        strings_value(Some(redirect_uris)).unwrap_or(Value::Null),
    );
    from_object(input, "OAuth client registration response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_tokens_treating_empty_values_as_absent() {
        let tokens = parse_oauth_tokens(Some(json!({
            "access_token": "a", "token_type": "Bearer", "refresh_token": "", "expires_in": null, "scope": ""
        })))
        .unwrap();
        assert_eq!(tokens, OAuthTokens::bearer("a"));
        let tokens = parse_oauth_tokens(Some(
            json!({ "access_token": "a", "token_type": "Bearer", "expires_in": "3600" }),
        ))
        .unwrap();
        assert_eq!(tokens.expires_in, Some(3600.0));
        assert_eq!(
            serde_json::to_value(&tokens).unwrap()["expires_in"],
            json!(3600)
        );
        assert!(
            parse_oauth_tokens(Some(
                json!({ "access_token": "a", "token_type": "Bearer", "expires_in": "x" })
            ))
            .is_err()
        );
        assert_eq!(
            parse_oauth_tokens(None).unwrap_err().to_string(),
            "Invalid OAuth token response"
        );
    }

    #[test]
    fn expires_in_follows_js_number_conversion() {
        // Expected values from Node's `Number(value)`.
        let cases = [
            (json!("0x1A"), 26.0),
            (json!(" 42\n"), 42.0),
            (json!("\u{a0}\u{feff}7\u{3000}"), 7.0),
            (json!("0b101"), 5.0),
            (json!("0o17"), 15.0),
            (json!("1e3"), 1000.0),
            (json!(".5"), 0.5),
            (json!("5."), 5.0),
            (json!("+.5e-1"), 0.05),
            (json!([]), 0.0),
            (json!([null]), 0.0),
            (json!(["0x10"]), 16.0),
            (json!([[" 8 "]]), 8.0),
            (json!(true), 1.0),
        ];
        for (value, expected) in cases {
            assert_eq!(js_number(&value), expected, "{value}");
            let tokens = parse_oauth_tokens(Some(
                json!({ "access_token": "a", "token_type": "Bearer", "expires_in": value }),
            ))
            .unwrap();
            assert_eq!(tokens.expires_in, Some(expected));
        }
        for value in [
            json!("-0x10"),
            json!("inf"),
            json!("1_000"),
            json!("1e"),
            json!("0x"),
            json!("\u{85} 1"),
            json!("Infinity"),
            json!([true]),
            json!([1, 2]),
            json!({}),
        ] {
            let error = parse_oauth_tokens(Some(
                json!({ "access_token": "a", "token_type": "Bearer", "expires_in": value }),
            ))
            .unwrap_err();
            assert_eq!(error.to_string(), "Invalid expires_in", "{value}");
        }
        assert_eq!(js_number(&json!("-Infinity")), f64::NEG_INFINITY);
        assert_eq!(js_number(&json!("")), 0.0);
        // An empty string counts as absent, like null.
        let tokens = parse_oauth_tokens(Some(
            json!({ "access_token": "a", "token_type": "Bearer", "expires_in": "" }),
        ))
        .unwrap();
        assert_eq!(tokens.expires_in, None);
    }

    #[test]
    fn validates_authorization_server_metadata() {
        let metadata = parse_authorization_server_metadata(json!({
            "issuer": "https://as.example",
            "authorization_endpoint": "https://as.example/authorize",
            "token_endpoint": "https://as.example/token",
            "registration_endpoint": "",
            "response_types_supported": ["code"],
            "custom": 1
        }))
        .unwrap();
        assert_eq!(metadata.registration_endpoint, None);
        assert_eq!(metadata.extra.get("custom"), Some(&json!(1)));
        let error = parse_authorization_server_metadata(json!({
            "issuer": "javascript:alert(1)",
            "authorization_endpoint": "https://as.example/authorize",
            "token_endpoint": "https://as.example/token",
            "response_types_supported": ["code"]
        }))
        .unwrap_err();
        assert_eq!(error.to_string(), "Invalid authorization server issuer");
    }

    #[test]
    fn parses_client_information() {
        let info = parse_client_information(
            json!({ "client_id": "c", "client_secret": "", "client_name": "x" }),
        )
        .unwrap();
        assert_eq!(info.client_secret, None);
        assert_eq!(info.redirect_uris(), Vec::<String>::new());
        assert!(info.has_redirect_uris());
        assert_eq!(info.extra.get("client_name"), Some(&json!("x")));
    }
}
