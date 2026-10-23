//! The OAuth 2.1 authorization code flow with PKCE for MCP servers: discovery, dynamic client
//! registration or Client ID Metadata Documents, code exchange, refresh, and step-up.
//! Adapted from modelcontextprotocol/typescript-sdk v1.29.0 (MIT, Copyright (c) 2024 Anthropic, PBC).

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use rand::RngCore;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::Url;

use super::discovery::{
    AuthorizationServerDiscoveryOptions, OAuthServerInfoOptions,
    discover_authorization_server_metadata, discover_oauth_server_info, parse_www_authenticate,
    select_resource,
};
use super::errors::OAuthError;
use super::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientInformationFull,
    OAuthClientMetadata, OAuthDiscoveryState, OAuthServerInfo, OAuthTokens,
    is_http_url, parse_client_information, parse_oauth_tokens,
};
use crate::abort::AbortSignal;
use crate::auth_provider::{
    AuthProvider, HttpRequest, McpFetch, UnauthorizedContext, default_fetch, set_header,
};
use crate::error::{Error, Result, join_url, parse_url};

/// Ordered `application/x-www-form-urlencoded` parameters (`URLSearchParams`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlSearchParams {
    pairs: Vec<(String, String)>,
}

impl UrlSearchParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_pairs<I: IntoIterator<Item = (K, V)>, K: Into<String>, V: Into<String>>(
        pairs: I,
    ) -> Self {
        Self {
            pairs: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    pub fn parse(input: &str) -> Self {
        Self {
            pairs: url::form_urlencoded::parse(input.as_bytes())
                .into_owned()
                .collect(),
        }
    }

    /// Replace the first value of `key` and remove the others, or append.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        let mut found = false;
        self.pairs.retain_mut(|(name, current)| {
            if name != key {
                return true;
            }
            if found {
                return false;
            }
            found = true;
            *current = value.clone();
            true
        });
        if !found {
            self.pairs.push((key.to_string(), value));
        }
    }

    pub fn append(&mut self, key: &str, value: impl Into<String>) {
        self.pairs.push((key.to_string(), value.into()));
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn delete(&mut self, key: &str) {
        self.pairs.retain(|(name, _)| name != key);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

impl std::fmt::Display for UrlSearchParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &self.pairs {
            serializer.append_pair(key, value);
        }
        f.write_str(&serializer.finish())
    }
}

/// `url.searchParams.set(key, value)`.
pub fn set_search_param(url: &mut Url, key: &str, value: &str) {
    let mut params = UrlSearchParams::parse(url.query().unwrap_or(""));
    params.set(key, value);
    url.set_query(Some(&params.to_string()));
}

/// `url.searchParams.get(key)`.
pub fn get_search_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

/// Custom client authentication for token requests: may set headers and form parameters.
pub type AddClientAuthentication = Arc<
    dyn Fn(
            &mut HeaderMap,
            &mut UrlSearchParams,
            &Url,
            Option<&AuthorizationServerMetadata>,
        ) -> Result<()>
        + Send
        + Sync,
>;

/// A Client ID Metadata Document: an https URL used as `client_id`, and a redirect URI it lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthClientMetadataDocument {
    pub url: String,
    pub redirect_url: String,
}

/// What to invalidate after the authorization server rejected stored credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidateCredentials {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

/// Storage and user interaction for the OAuth flow of one MCP server.
#[async_trait]
pub trait OAuthClientProvider: Send + Sync {
    fn redirect_url(&self) -> String;
    fn client_metadata(&self) -> OAuthClientMetadata;

    /// Client ID Metadata Document to identify as instead of registering dynamically, or `None` to
    /// register. Called when no client information is stored; the document is not stored.
    /// `metadata` is `None` when the authorization server has none; check
    /// `client_id_metadata_document_supported`.
    fn client_metadata_document(
        &self,
        _metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<OAuthClientMetadataDocument>> {
        Ok(None)
    }

    /// The OAuth `state` parameter, if the provider uses one.
    async fn state(&self) -> Result<Option<String>> {
        Ok(None)
    }

    async fn client_information(&self) -> Result<Option<OAuthClientInformation>>;

    /// Whether [`save_client_information`](Self::save_client_information) persists registrations.
    /// Providers that cannot persist them return `false`, and the flow does not register.
    fn persists_client_information(&self) -> bool {
        true
    }

    async fn save_client_information(&self, information: OAuthClientInformation) -> Result<()>;
    async fn tokens(&self) -> Result<Option<OAuthTokens>>;
    async fn save_tokens(&self, tokens: OAuthTokens) -> Result<()>;
    async fn redirect_to_authorization(&self, url: Url) -> Result<()>;
    async fn save_code_verifier(&self, verifier: String) -> Result<()>;
    async fn code_verifier(&self) -> Result<String>;

    fn add_client_authentication(&self) -> Option<AddClientAuthentication> {
        None
    }

    async fn invalidate_credentials(&self, _kind: InvalidateCredentials) -> Result<()> {
        Ok(())
    }

    async fn save_discovery_state(&self, _state: OAuthDiscoveryState) -> Result<()> {
        Ok(())
    }

    async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>> {
        Ok(None)
    }
}

#[derive(Clone)]
pub struct OAuthFlowOptions {
    pub server_url: Url,
    pub authorization_code: Option<String>,
    /// `iss` parameter of the authorization response that delivered `authorization_code` (RFC 9207).
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<Url>,
    /// Authorization server metadata document to use instead of discovery, for servers that
    /// advertise a wrong authorization server or none. It is trusted as configured. Must use https,
    /// except on loopback.
    pub authorization_server_metadata_url: Option<Url>,
    pub fetch: Option<McpFetch>,
    /// Aborts every request of the flow. Requests have no time limit of their own.
    pub signal: Option<AbortSignal>,
    pub skip_issuer_validation: bool,
    /// Go straight to the authorization redirect instead of refreshing stored tokens, for example
    /// when the server asks for scopes the current grant lacks (a refresh keeps the old scope).
    pub skip_refresh: bool,
}

impl OAuthFlowOptions {
    pub fn new(server_url: Url) -> Self {
        Self {
            server_url,
            authorization_code: None,
            iss: None,
            scope: None,
            resource_metadata_url: None,
            authorization_server_metadata_url: None,
            fetch: None,
            signal: None,
            skip_issuer_validation: false,
            skip_refresh: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthFlowResult {
    Authorized,
    Redirect,
}

impl OAuthFlowResult {
    /// `"AUTHORIZED"` or `"REDIRECT"`.
    pub fn as_str(self) -> &'static str {
        match self {
            OAuthFlowResult::Authorized => "AUTHORIZED",
            OAuthFlowResult::Redirect => "REDIRECT",
        }
    }
}

#[derive(Clone)]
pub struct TokenRequestOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_information: OAuthClientInformation,
    pub resource: Option<String>,
    pub add_client_authentication: Option<AddClientAuthentication>,
    pub fetch: Option<McpFetch>,
    pub signal: Option<AbortSignal>,
}

impl TokenRequestOptions {
    pub fn new(client_information: OAuthClientInformation) -> Self {
        Self {
            metadata: None,
            client_information,
            resource: None,
            add_client_authentication: None,
            fetch: None,
            signal: None,
        }
    }
}

fn loopback(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The OpenID Connect `application_type` for `redirect_uris` (MCP SEP-837). Without one, OpenID
/// Connect servers assume `web`, which rejects http loopback redirect URIs. Loopback hosts and
/// custom schemes are native (RFC 8252).
fn application_type(redirect_uris: &[String]) -> &'static str {
    let native = redirect_uris.iter().any(|uri| {
        let Ok(url) = Url::parse(uri) else {
            return false;
        };
        (url.scheme() != "http" && url.scheme() != "https") || url.host_str().is_some_and(loopback)
    });
    if native { "native" } else { "web" }
}

fn secure_endpoint(value: &Url) -> Result<Url> {
    if value.scheme() != "https" && !value.host_str().is_some_and(loopback) {
        return Err(Error::OAuthInsecureEndpoint {
            endpoint: value.to_string(),
        });
    }
    Ok(value.clone())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientAuthMethod {
    Basic,
    Post,
    None,
}

fn select_client_auth_method(
    information: &OAuthClientInformation,
    supported: &[String],
) -> ClientAuthMethod {
    let has = |method: &str| supported.iter().any(|entry| entry == method);
    if let Some(hinted) = information.token_endpoint_auth_method()
        && ["client_secret_basic", "client_secret_post", "none"].contains(&hinted)
        && (supported.is_empty() || has(hinted))
    {
        return match hinted {
            "client_secret_basic" => ClientAuthMethod::Basic,
            "client_secret_post" => ClientAuthMethod::Post,
            _ => ClientAuthMethod::None,
        };
    }
    let secret = information
        .client_secret
        .as_deref()
        .is_some_and(|secret| !secret.is_empty());
    if supported.is_empty() {
        return if secret {
            ClientAuthMethod::Basic
        } else {
            ClientAuthMethod::None
        };
    }
    if secret && has("client_secret_basic") {
        return ClientAuthMethod::Basic;
    }
    if secret && has("client_secret_post") {
        return ClientAuthMethod::Post;
    }
    if has("none") {
        return ClientAuthMethod::None;
    }
    if secret {
        ClientAuthMethod::Post
    } else {
        ClientAuthMethod::None
    }
}

fn apply_client_authentication(
    method: ClientAuthMethod,
    information: &OAuthClientInformation,
    headers: &mut HeaderMap,
    params: &mut UrlSearchParams,
) -> Result<()> {
    let secret = information
        .client_secret
        .as_deref()
        .filter(|secret| !secret.is_empty());
    if method == ClientAuthMethod::Basic {
        let secret =
            secret.ok_or_else(|| Error::other("client_secret_basic requires a client secret"))?;
        let credentials = STANDARD.encode(format!("{}:{}", information.client_id, secret));
        set_header(headers, "Authorization", &format!("Basic {credentials}"));
    } else {
        params.set("client_id", information.client_id.clone());
        if method == ClientAuthMethod::Post
            && let Some(secret) = secret
        {
            params.set("client_secret", secret);
        }
    }
    Ok(())
}

fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

#[derive(Clone, Debug, Default)]
pub struct StartAuthorizationOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_information: Option<OAuthClientInformation>,
    pub redirect_url: String,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub resource: Option<String>,
}

/// Build the authorization URL with a fresh PKCE verifier.
pub fn start_authorization(
    authorization_server_url: &str,
    options: &StartAuthorizationOptions,
) -> Result<(Url, String)> {
    if let Some(metadata) = &options.metadata {
        if !metadata
            .response_types_supported
            .iter()
            .any(|kind| kind == "code")
        {
            return Err(Error::other(
                "Authorization server does not support authorization codes",
            ));
        }
        if let Some(methods) = &metadata.code_challenge_methods_supported
            && !methods.iter().any(|method| method == "S256")
        {
            return Err(Error::other(
                "Authorization server does not support PKCE S256",
            ));
        }
    }
    let mut url = match &options.metadata {
        Some(metadata) => parse_url(&metadata.authorization_endpoint)?,
        None => join_url(&parse_url(authorization_server_url)?, "/authorize")?,
    };
    // Callers open this URL with the platform URL handler, which also launches files and custom protocols.
    if !is_http_url(&url) {
        return Err(Error::other(format!("Invalid authorization endpoint {url}")));
    }
    let (verifier, challenge) = pkce();
    let client_id = options
        .client_information
        .as_ref()
        .map(|client| client.client_id.clone())
        .unwrap_or_default();
    set_search_param(&mut url, "response_type", "code");
    set_search_param(&mut url, "client_id", &client_id);
    set_search_param(&mut url, "code_challenge", &challenge);
    set_search_param(&mut url, "code_challenge_method", "S256");
    set_search_param(&mut url, "redirect_uri", &options.redirect_url);
    if let Some(state) = options.state.as_deref().filter(|state| !state.is_empty()) {
        set_search_param(&mut url, "state", state);
    }
    if let Some(scope) = options.scope.as_deref().filter(|scope| !scope.is_empty()) {
        set_search_param(&mut url, "scope", scope);
        if scope
            .split_whitespace()
            .any(|entry| entry == "offline_access")
        {
            set_search_param(&mut url, "prompt", "consent");
        }
    }
    if let Some(resource) = options
        .resource
        .as_deref()
        .filter(|resource| !resource.is_empty())
    {
        set_search_param(&mut url, "resource", resource);
    }
    Ok((url, verifier))
}

async fn token_request(
    authorization_server_url: &str,
    options: &TokenRequestOptions,
    mut params: UrlSearchParams,
) -> Result<OAuthTokens> {
    let endpoint = match &options.metadata {
        Some(metadata) => parse_url(&metadata.token_endpoint)?,
        None => join_url(&parse_url(authorization_server_url)?, "/token")?,
    };
    let url = secure_endpoint(&endpoint)?;
    let mut headers = HeaderMap::new();
    set_header(&mut headers, "Accept", "application/json");
    set_header(
        &mut headers,
        "content-type",
        "application/x-www-form-urlencoded",
    );
    if let Some(resource) = &options.resource {
        params.set("resource", resource.clone());
    }
    if let Some(add) = &options.add_client_authentication {
        add(&mut headers, &mut params, &url, options.metadata.as_ref())?;
    } else {
        let supported = options
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.token_endpoint_auth_methods_supported.clone())
            .unwrap_or_default();
        apply_client_authentication(
            select_client_auth_method(&options.client_information, &supported),
            &options.client_information,
            &mut headers,
            &mut params,
        )?;
    }
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: Some(params.to_string().into()),
        signal: options.signal.clone(),
    };
    let response = fetch(request).await?;
    let (ok, status) = (response.ok(), response.status());
    let text = response.text().await?;
    let value: Option<Value> = pi_json::js_json_parse(&text).ok();
    // Servers may report OAuth errors with any status, so check the body before the status.
    if let Some(Value::Object(object)) = &value
        && let Some(Value::String(code)) = object.get("error")
    {
        let message = match object.get("error_description") {
            Some(Value::String(description)) => description.clone(),
            _ => code.clone(),
        };
        let uri = object
            .get("error_uri")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(Error::OAuth(OAuthError::new(code.clone(), message, uri)));
    }
    if !ok {
        return Err(Error::OAuth(OAuthError::new(
            "server_error",
            format!("HTTP {status}: {text}"),
            None,
        )));
    }
    parse_oauth_tokens(value)
}

#[derive(Clone, Default)]
pub struct RegisterClientOptions {
    pub metadata: Option<AuthorizationServerMetadata>,
    pub client_metadata: OAuthClientMetadata,
    pub scope: Option<String>,
    pub fetch: Option<McpFetch>,
    pub signal: Option<AbortSignal>,
}

/// Dynamic client registration (RFC 7591).
pub async fn register_client(
    authorization_server_url: &str,
    options: RegisterClientOptions,
) -> Result<OAuthClientInformationFull> {
    let endpoint = options
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.registration_endpoint.clone());
    if options.metadata.is_some() && endpoint.is_none() {
        return Err(Error::other(
            "Authorization server does not support dynamic client registration",
        ));
    }
    let url = match endpoint {
        Some(endpoint) => parse_url(&endpoint)?,
        None => join_url(&parse_url(authorization_server_url)?, "/register")?,
    };
    let mut body = match serde_json::to_value(&options.client_metadata) {
        Ok(Value::Object(body)) => body,
        _ => Map::new(),
    };
    let application = options
        .client_metadata
        .application_type
        .clone()
        .unwrap_or_else(|| application_type(&options.client_metadata.redirect_uris).to_string());
    body.insert("application_type".into(), Value::String(application));
    if let Some(scope) = options.scope.as_deref().filter(|scope| !scope.is_empty()) {
        body.insert("scope".into(), Value::String(scope.to_string()));
    }
    let fetch = options.fetch.clone().unwrap_or_else(default_fetch);
    let request = HttpRequest::new("POST", url)
        .header("Accept", "application/json")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&Value::Object(body)).unwrap_or_default())
        .signal(options.signal.clone());
    let response = fetch(request).await?;
    if !response.ok() {
        let status = response.status();
        let body = response.text().await?;
        return Err(Error::OAuthRegistration { status, body });
    }
    parse_client_information(response.json().await?)
}

pub async fn exchange_authorization_code(
    authorization_server_url: &str,
    options: &TokenRequestOptions,
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> Result<OAuthTokens> {
    let params = UrlSearchParams::from_pairs([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_url),
    ]);
    token_request(authorization_server_url, options, params).await
}

/// Refresh the tokens; the old refresh token is kept when the response carries none.
pub async fn refresh_authorization(
    authorization_server_url: &str,
    options: &TokenRequestOptions,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let params = UrlSearchParams::from_pairs([
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]);
    let mut tokens = token_request(authorization_server_url, options, params).await?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    Ok(tokens)
}

fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none()
        && let Some(scope) = scope.filter(|scope| !scope.is_empty())
    {
        tokens.scope = Some(scope.to_string());
    }
    tokens
}

/// Scopes for a step-up authorization: the challenged scopes plus the ones granted so far, since a
/// challenge may list only the missing scopes and a token with just those would lose access the old
/// one had (SEP-2350). Without challenged scopes, `None` lets the flow pick its default.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged.filter(|scope| !scope.is_empty())?;
    let mut scopes: Vec<&str> = Vec::new();
    for scope in [granted.unwrap_or(""), challenged] {
        for entry in scope.split_whitespace() {
            if !scopes.contains(&entry) {
                scopes.push(entry);
            }
        }
    }
    Some(scopes.join(" "))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(str::to_string)
}

async fn run_flow(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult> {
    let server_url = options.server_url.clone();
    let metadata_url = options
        .authorization_server_metadata_url
        .as_ref()
        .map(secure_endpoint)
        .transpose()?;
    // With a configured metadata URL, discovery is not cached, so changing the URL applies at once.
    let cached = if metadata_url.is_some() {
        None
    } else {
        provider.discovery_state().await?
    };
    let discovered = match cached.filter(|cached| !cached.authorization_server_url.is_empty()) {
        Some(cached) => {
            let metadata = match cached.authorization_server_metadata {
                Some(metadata) => Some(metadata),
                None => {
                    discover_authorization_server_metadata(
                        &cached.authorization_server_url,
                        AuthorizationServerDiscoveryOptions {
                            fetch: options.fetch.clone(),
                            protocol_version: None,
                            skip_issuer_validation: options.skip_issuer_validation,
                            signal: options.signal.clone(),
                        },
                    )
                    .await?
                }
            };
            OAuthServerInfo {
                authorization_server_url: cached.authorization_server_url,
                authorization_server_metadata: metadata,
                resource_metadata: cached.resource_metadata,
            }
        }
        None => {
            discover_oauth_server_info(
                &server_url,
                OAuthServerInfoOptions {
                    resource_metadata_url: options.resource_metadata_url.clone(),
                    authorization_server_metadata_url: metadata_url.clone(),
                    fetch: options.fetch.clone(),
                    skip_issuer_validation: options.skip_issuer_validation,
                    signal: options.signal.clone(),
                },
            )
            .await?
        }
    };
    if metadata_url.is_none() {
        provider
            .save_discovery_state(OAuthDiscoveryState {
                authorization_server_url: discovered.authorization_server_url.clone(),
                authorization_server_metadata: discovered.authorization_server_metadata.clone(),
                resource_metadata: discovered.resource_metadata.clone(),
                resource_metadata_url: options.resource_metadata_url.as_ref().map(Url::to_string),
            })
            .await?;
    }
    let metadata = discovered.authorization_server_metadata.clone();
    let resource = select_resource(&server_url, discovered.resource_metadata.as_ref())?;
    // An empty scope (for example from `scopes_supported: []`) falls through to the next source.
    let scope = non_empty(options.scope.as_deref())
        .or_else(|| {
            non_empty(
                discovered
                    .resource_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.scopes_supported.as_ref())
                    .map(|scopes| scopes.join(" "))
                    .as_deref(),
            )
        })
        .or_else(|| non_empty(provider.client_metadata().scope.as_deref()));
    let stored = provider.client_information().await?;
    let client_document = if stored.is_some() {
        None
    } else {
        provider.client_metadata_document(metadata.as_ref())?
    };
    if let Some(document) = &client_document {
        let url = parse_url(&document.url)?;
        if url.scheme() != "https" || url.path() == "/" {
            return Err(Error::other("Invalid OAuth client metadata URL"));
        }
    }
    let mut client = stored.or_else(|| {
        client_document
            .as_ref()
            .map(|document| OAuthClientInformation::new(&document.url))
    });
    if client.is_none() {
        if options.authorization_code.is_some() {
            return Err(Error::other(
                "OAuth client information is missing during code exchange",
            ));
        }
        if !provider.persists_client_information() {
            return Err(Error::other("OAuth client information cannot be persisted"));
        }
        let registered = register_client(
            &discovered.authorization_server_url,
            RegisterClientOptions {
                metadata: metadata.clone(),
                client_metadata: provider.client_metadata(),
                scope: scope.clone(),
                fetch: options.fetch.clone(),
                signal: options.signal.clone(),
            },
        )
        .await?;
        provider.save_client_information(registered.clone()).await?;
        client = Some(registered);
    }
    let client = client.expect("client information is stored, documented, or registered");
    // The document's redirect URI may differ from the provider's, for example by a server-specific path.
    let redirect_url = client_document
        .as_ref()
        .map(|document| document.redirect_url.clone())
        .unwrap_or_else(|| provider.redirect_url());
    let token_options = TokenRequestOptions {
        metadata: metadata.clone(),
        client_information: client.clone(),
        resource: resource.clone(),
        add_client_authentication: provider.add_client_authentication(),
        fetch: options.fetch.clone(),
        signal: options.signal.clone(),
    };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to this one. Without
        // metadata there is no issuer to compare with, so any `iss` is rejected.
        let promised = metadata
            .as_ref()
            .and_then(|metadata| metadata.authorization_response_iss_parameter_supported)
            .unwrap_or(false);
        if options.iss.is_some() || promised {
            let expected = metadata.as_ref().map(|metadata| metadata.issuer.clone());
            if options.iss != expected {
                return Err(Error::OAuthIssuerMismatch {
                    expected,
                    received: options.iss.clone(),
                });
            }
        }
        let verifier = provider.code_verifier().await?;
        let tokens = exchange_authorization_code(
            &discovered.authorization_server_url,
            &token_options,
            code,
            &verifier,
            &redirect_url,
        )
        .await?;
        // A response without `scope` grants the requested scope (RFC 6749 §5.1). Recorded so a
        // step-up can keep it. Callers pass the options of the authorization request, so `scope`
        // is what was requested.
        provider
            .save_tokens(with_scope(tokens, scope.as_deref()))
            .await?;
        return Ok(OAuthFlowResult::Authorized);
    }
    let existing = if options.skip_refresh {
        None
    } else {
        provider.tokens().await?
    };
    if let Some(existing) = existing
        && let Some(refresh_token) = existing
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
    {
        match refresh_authorization(
            &discovered.authorization_server_url,
            &token_options,
            refresh_token,
        )
        .await
        {
            Ok(tokens) => {
                // A refresh without `scope` keeps the scope of the grant (RFC 6749 §6).
                provider
                    .save_tokens(with_scope(tokens, existing.scope.as_deref()))
                    .await?;
                return Ok(OAuthFlowResult::Authorized);
            }
            Err(error) => {
                if options.signal.as_ref().is_some_and(AbortSignal::is_aborted)
                    || matches!(error, Error::OAuthInsecureEndpoint { .. })
                {
                    return Err(error);
                }
                if let Error::OAuth(oauth) = &error
                    && oauth.code != "server_error"
                {
                    return Err(error);
                }
            }
        }
    }
    let state = provider.state().await?;
    let (authorization_url, code_verifier) = start_authorization(
        &discovered.authorization_server_url,
        &StartAuthorizationOptions {
            metadata,
            client_information: Some(client),
            redirect_url,
            scope,
            state,
            resource,
        },
    )?;
    provider.save_code_verifier(code_verifier).await?;
    provider
        .redirect_to_authorization(authorization_url)
        .await?;
    Ok(OAuthFlowResult::Redirect)
}

/// Run the authorization flow: exchange `authorization_code` when given, otherwise refresh stored
/// tokens or start a new authorization (`Redirect`). Rejected client credentials or grants are
/// invalidated and the flow runs once more.
pub async fn authorize_mcp(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult> {
    match run_flow(provider, options).await {
        Err(Error::OAuth(error))
            if error.code == "invalid_client" || error.code == "unauthorized_client" =>
        {
            provider
                .invalidate_credentials(InvalidateCredentials::All)
                .await?;
            run_flow(provider, options).await
        }
        Err(Error::OAuth(error)) if error.code == "invalid_grant" => {
            provider
                .invalidate_credentials(InvalidateCredentials::Tokens)
                .await?;
            run_flow(provider, options).await
        }
        result => result,
    }
}

type SharedRecovery = Shared<BoxFuture<'static, Result<()>>>;

struct OAuthAuthProvider {
    provider: Arc<dyn OAuthClientProvider>,
    in_flight: Arc<Mutex<Option<SharedRecovery>>>,
}

impl AuthProvider for OAuthAuthProvider {
    fn token(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            Ok(self
                .provider
                .tokens()
                .await?
                .map(|tokens| tokens.access_token))
        })
    }

    fn on_unauthorized(&self, context: UnauthorizedContext) -> Option<BoxFuture<'_, Result<()>>> {
        Some(Box::pin(async move {
            let challenge =
                parse_www_authenticate(context.response.header("www-authenticate").as_deref());
            let insufficient_scope = challenge.error.as_deref() == Some("insufficient_scope");
            if !insufficient_scope && self.in_flight.lock().is_none() && context.token.is_some() {
                let current = self
                    .provider
                    .tokens()
                    .await?
                    .map(|tokens| tokens.access_token);
                if current.is_some() && current != context.token {
                    return Ok(());
                }
            }
            let recovery = {
                let mut in_flight = self.in_flight.lock();
                match in_flight.as_ref() {
                    Some(recovery) => recovery.clone(),
                    None => {
                        let (provider, slot) = (self.provider.clone(), self.in_flight.clone());
                        let future: BoxFuture<'static, Result<()>> = Box::pin(async move {
                            let result = async {
                                let granted = if insufficient_scope {
                                    provider.tokens().await?
                                } else {
                                    None
                                };
                                let options = OAuthFlowOptions {
                                    resource_metadata_url: challenge.resource_metadata_url.clone(),
                                    scope: if insufficient_scope {
                                        step_up_scope(
                                            granted
                                                .as_ref()
                                                .and_then(|tokens| tokens.scope.as_deref()),
                                            challenge.scope.as_deref(),
                                        )
                                    } else {
                                        challenge.scope.clone()
                                    },
                                    fetch: Some(context.fetch.clone()),
                                    skip_refresh: insufficient_scope,
                                    ..OAuthFlowOptions::new(context.server_url.clone())
                                };
                                match authorize_mcp(provider.as_ref(), &options).await? {
                                    OAuthFlowResult::Redirect => {
                                        Err(Error::OAuthAuthorizationRequired)
                                    }
                                    OAuthFlowResult::Authorized => Ok(()),
                                }
                            }
                            .await;
                            slot.lock().take();
                            result
                        });
                        let shared = future.shared();
                        *in_flight = Some(shared.clone());
                        shared
                    }
                }
            };
            recovery.await
        }))
    }
}

/// Auth provider for `StreamableHttpTransport`. After a 401 it refreshes the tokens, or fails with
/// [`Error::OAuthAuthorizationRequired`] when the user has to authorize (again). Concurrent 401s
/// share one refresh, and a request whose token was already replaced is just retried: with
/// rotating refresh tokens, a second refresh with the old refresh token would fail and discard the
/// new grant.
pub fn adapt_oauth_provider(provider: Arc<dyn OAuthClientProvider>) -> Arc<dyn AuthProvider> {
    Arc::new(OAuthAuthProvider {
        provider,
        in_flight: Arc::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_step_up_scopes() {
        assert_eq!(
            step_up_scope(Some("repo read:org"), Some("repo admin")).as_deref(),
            Some("repo read:org admin")
        );
        assert_eq!(step_up_scope(Some("repo"), None), None);
        assert_eq!(step_up_scope(None, Some("a  b")).as_deref(), Some("a b"));
    }

    #[test]
    fn search_params_set_replaces_values() {
        let mut params = UrlSearchParams::parse("a=1&b=2&a=3");
        params.set("a", "x y");
        assert_eq!(params.to_string(), "a=x+y&b=2");
        let mut url = Url::parse("https://as.example/authorize?existing=1").unwrap();
        set_search_param(&mut url, "redirect_uri", "http://127.0.0.1:1/callback");
        assert_eq!(
            get_search_param(&url, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:1/callback")
        );
        assert_eq!(get_search_param(&url, "existing").as_deref(), Some("1"));
    }

    #[test]
    fn selects_client_authentication() {
        let public = OAuthClientInformation::new("c");
        let confidential = OAuthClientInformation::new("c").with_secret("s");
        let supported =
            |methods: &[&str]| methods.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        assert!(select_client_auth_method(&public, &[]) == ClientAuthMethod::None);
        assert!(select_client_auth_method(&confidential, &[]) == ClientAuthMethod::Basic);
        assert!(
            select_client_auth_method(&confidential, &supported(&["client_secret_post"]))
                == ClientAuthMethod::Post
        );
        let mut hinted = confidential.clone();
        hinted.extra.insert(
            "token_endpoint_auth_method".into(),
            Value::String("client_secret_post".into()),
        );
        assert!(select_client_auth_method(&hinted, &[]) == ClientAuthMethod::Post);
        assert_eq!(
            application_type(&["com.example.app:/callback".to_string()]),
            "native"
        );
        assert_eq!(
            application_type(&["https://app.example/callback".to_string()]),
            "web"
        );
    }
}
