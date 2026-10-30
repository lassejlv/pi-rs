//! OpenAI Responses API token sharing through Sign in with ChatGPT
//! (port of `auth/oauth/openai-chatgpt.ts`).
//!
//! This public-client flow uses no client secret and sends the resulting user access token
//! directly to api.openai.com.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use rand::RngCore;
use serde_json::{Map, Value, json};
use crate::utils::wasm::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::callback_server::{SharedListener, read_request_line, send_page};
use super::http::{OAuthFetch, OAuthHttpRequest, current_oauth_fetch, send};
use super::pkce::{base64url_encode, generate_pkce};
use crate::auth::types::{
    AuthEvent, AuthPrompt, AuthPromptKind, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
    ProviderAuthInteraction,
};
use crate::utils::oauth_page::{oauth_error_html, oauth_success_html};
use crate::utils::provider_env::get_provider_env_value;

/// Every login registers a new client with this ID; OpenAI returns the issued client ID.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Refresh this long before the real expiry.
const EXPIRY_MARGIN_MS: f64 = 3.0 * 60.0 * 1000.0;

fn scope() -> String {
    format!("openid profile email offline_access resource.invoke {DIRECT_TOKEN_SCOPE}")
}

fn callback_host() -> String {
    get_provider_env_value("PI_OAUTH_CALLBACK_HOST", None).unwrap_or_else(|| "127.0.0.1".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorizationResult {
    code: String,
    client_id: String,
}

fn random_value() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64url_encode(&bytes)
}

fn authorization_result_from_callback(
    url: &url::Url,
    expected_state: &str,
) -> anyhow::Result<AuthorizationResult> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let Some(code) = param("code").filter(|c| !c.is_empty()) else {
        anyhow::bail!("Missing authorization code")
    };
    let Some(state) = param("state").filter(|s| !s.is_empty()) else {
        anyhow::bail!("Missing OAuth state")
    };
    if state != expected_state {
        anyhow::bail!("OAuth state mismatch");
    }
    let Some(client_id) = param("client_id")
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
    else {
        anyhow::bail!("OpenAI OAuth registration callback did not contain an issued client ID");
    };
    Ok(AuthorizationResult { code, client_id })
}

fn authorization_result_from_manual_input(
    input: &str,
    expected_state: &str,
) -> anyhow::Result<AuthorizationResult> {
    let Ok(url) = url::Url::parse(input.trim()) else {
        anyhow::bail!("Paste the full callback URL from the browser")
    };
    let expected = url::Url::parse(REDIRECT_URI).expect("valid redirect URI");
    if url.origin() != expected.origin() || url.path() != expected.path() {
        anyhow::bail!("The pasted callback URL must start with {REDIRECT_URI}");
    }
    if let Some((_, error)) = url.query_pairs().find(|(k, _)| k == "error") {
        anyhow::bail!("ChatGPT authorization failed: {error}");
    }
    authorization_result_from_callback(&url, expected_state)
}

/// The loopback server of this flow (not shared with the other browser flows).
struct CallbackServer {
    result: oneshot::Receiver<Result<AuthorizationResult, String>>,
    shutdown: CancellationToken,
    listener: SharedListener,
    connections: Arc<Mutex<Vec<crate::utils::wasm::AbortHandle>>>,
}

fn close_server(
    shutdown: &CancellationToken,
    listener: &SharedListener,
    connections: &Mutex<Vec<crate::utils::wasm::AbortHandle>>,
) {
    listener.close();
    shutdown.cancel();
    // Also drop spare connections the browser opened ahead of time, so a later login's
    // callback is not handled (and rejected) by this server.
    for handle in connections.lock().drain(..) {
        handle.abort();
    }
}

async fn start_callback_server(expected_state: String) -> anyhow::Result<CallbackServer> {
    let listener = match TcpListener::bind((callback_host().as_str(), CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => anyhow::bail!(
            "Port {CALLBACK_PORT} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again."
        ),
        Err(error) => return Err(error.into()),
    };
    let listener = SharedListener::new(listener);
    let accept_listener = listener.clone();
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let shutdown = CancellationToken::new();
    let connections: Arc<Mutex<Vec<crate::utils::wasm::AbortHandle>>> = Arc::default();
    let accept_shutdown = shutdown.clone();
    let accept_connections = connections.clone();
    crate::utils::wasm::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = accept_listener.accept() => accepted,
                _ = accept_shutdown.cancelled() => return,
            };
            let Some(accepted) = accepted else { return };
            let Ok(mut socket) = accepted else {
                if let Some(tx) = tx.lock().take() {
                    let _ = tx.send(Err("OpenAI OAuth callback server failed".into()));
                }
                return;
            };
            let tx = tx.clone();
            let expected_state = expected_state.clone();
            let handle = crate::utils::wasm::spawn(async move {
                let Some((_, target)) = read_request_line(&mut socket).await else {
                    return;
                };
                let Ok(url) = url::Url::parse(REDIRECT_URI).and_then(|base| base.join(&target))
                else {
                    send_page(
                        &mut socket,
                        500,
                        &oauth_error_html("Internal error while processing the callback.", None),
                        false,
                    )
                    .await;
                    return;
                };
                if url.path() != CALLBACK_PATH {
                    send_page(
                        &mut socket,
                        404,
                        &oauth_error_html("Callback route not found.", None),
                        false,
                    )
                    .await;
                    return;
                }
                if let Some((_, error)) = url.query_pairs().find(|(k, _)| k == "error") {
                    let error = error.into_owned();
                    send_page(
                        &mut socket,
                        400,
                        &oauth_error_html(
                            "ChatGPT was not connected.",
                            Some(&format!("Error: {error}")),
                        ),
                        false,
                    )
                    .await;
                    if let Some(tx) = tx.lock().take() {
                        let _ = tx.send(Err(format!("ChatGPT authorization failed: {error}")));
                    }
                    return;
                }
                match authorization_result_from_callback(&url, &expected_state) {
                    Err(error) => {
                        send_page(
                            &mut socket,
                            400,
                            &oauth_error_html(&error.to_string(), None),
                            false,
                        )
                        .await;
                    }
                    Ok(result) => {
                        send_page(
                            &mut socket,
                            200,
                            &oauth_success_html(
                                "ChatGPT authentication completed. You can close this window.",
                            ),
                            false,
                        )
                        .await;
                        if let Some(tx) = tx.lock().take() {
                            let _ = tx.send(Ok(result));
                        }
                    }
                }
            });
            accept_connections.lock().push(handle.abort_handle());
        }
    });
    Ok(CallbackServer {
        result: rx,
        shutdown,
        listener,
        connections,
    })
}

async fn request_token(
    fetch: &OAuthFetch,
    fields: &[(&str, &str)],
    signal: &CancellationToken,
) -> anyhow::Result<Map<String, Value>> {
    let request = OAuthHttpRequest::post(TOKEN_URL)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .form(fields);
    let response = send(fetch, request, Some(signal), None).await?;
    if !response.ok() {
        let text = if response.body.is_empty() {
            response.status_text.clone()
        } else {
            response.body.clone()
        };
        anyhow::bail!(
            "OpenAI OAuth token request failed ({}): {text}",
            response.status
        );
    }
    match response.json()? {
        Value::Object(map) => Ok(map),
        _ => anyhow::bail!("OpenAI OAuth token response must be an object"),
    }
}

fn require_token_string(token: &Map<String, Value>, field: &str) -> anyhow::Result<String> {
    match token.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => anyhow::bail!("OpenAI OAuth token response has invalid {field}"),
    }
}

fn credential_from_token_response(
    token: &Map<String, Value>,
    client_id: &str,
) -> anyhow::Result<OAuthCredential> {
    let access = require_token_string(token, "access_token")?;
    let refresh = require_token_string(token, "refresh_token")?;
    let scope = require_token_string(token, "scope")?;
    let Some(expires_in) = token
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|e| e.is_finite() && *e > 0.0)
    else {
        anyhow::bail!("OpenAI OAuth token response has invalid expires_in");
    };
    let scopes: Vec<String> = scope.split_whitespace().map(str::to_string).collect();
    if !scopes.iter().any(|s| s == DIRECT_TOKEN_SCOPE) {
        anyhow::bail!("OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}");
    }
    let mut extra = Map::new();
    extra.insert("clientId".into(), json!(client_id));
    extra.insert("scopes".into(), json!(scopes));
    Ok(OAuthCredential {
        access,
        refresh,
        expires: (crate::utils::now_ms() as f64 + expires_in * 1000.0 - EXPIRY_MARGIN_MS) as i64,
        extra,
    })
}

async fn exchange_authorization_code(
    fetch: &OAuthFetch,
    code: &str,
    verifier: &str,
    client_id: &str,
    signal: &CancellationToken,
) -> anyhow::Result<OAuthCredential> {
    let token = request_token(
        fetch,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
        ],
        signal,
    )
    .await?;
    // Pi does not use the ID token; its presence is part of the token-response contract.
    if !matches!(token.get("id_token"), Some(Value::String(s)) if !s.trim().is_empty()) {
        anyhow::bail!("OpenAI OAuth token response did not contain an ID token");
    }
    credential_from_token_response(&token, client_id)
}

async fn refresh_access_token(
    fetch: &OAuthFetch,
    credential: &OAuthCredential,
    signal: &CancellationToken,
) -> anyhow::Result<OAuthCredential> {
    let client_id = match credential.extra.get("clientId") {
        Some(Value::String(id)) if !id.trim().is_empty() => id.clone(),
        _ => anyhow::bail!(
            "Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT"
        ),
    };
    let token = request_token(
        fetch,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("refresh_token", &credential.refresh),
            ("resource", RESOURCE),
        ],
        signal,
    )
    .await?;
    credential_from_token_response(&token, &client_id)
}

/// OpenAI identifies each installation ("agent host") by a stable `urn:uuid:<uuid>` URI.
fn agent_host_id(device_id: Option<String>) -> anyhow::Result<String> {
    static UUID: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
            .unwrap()
    });
    match device_id {
        Some(id) if UUID.is_match(&id) => Ok(format!("urn:uuid:{}", id.to_lowercase())),
        _ => {
            anyhow::bail!("Sign in with ChatGPT requires a device ID (UUID) for this installation")
        }
    }
}

/// The authorization URL for a login.
fn authorization_url(
    host_id: &str,
    agent_name: &str,
    state: &str,
    challenge: &str,
    nonce: &str,
) -> String {
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid authorize URL");
    url.query_pairs_mut()
        .append_pair("client_id", DYNAMIC_CLIENT_ID)
        .append_pair("agent_name_hint", agent_name)
        .append_pair("ext_agent_host_id", host_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("resource", RESOURCE)
        .append_pair("scope", &scope())
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("nonce", nonce);
    url.to_string()
}

async fn login_openai_chatgpt(
    interaction: ProviderAuthInteraction,
    options: Option<LoginOptions>,
) -> anyhow::Result<OAuthCredential> {
    let fetch = current_oauth_fetch();
    let host_id = agent_host_id(
        options
            .as_ref()
            .and_then(|o| o.get_device_id.as_ref())
            .map(|f| f()),
    )?;
    let pkce = generate_pkce();
    let state = random_value();
    let nonce = random_value();
    let callback = start_callback_server(state.clone()).await?;
    let agent_name = options
        .as_ref()
        .and_then(|o| o.agent_name.clone())
        .unwrap_or_else(|| AGENT_NAME_HINT.into());
    interaction.interaction.notify(AuthEvent::AuthUrl {
        url: authorization_url(&host_id, &agent_name, &state, &pkce.challenge, &nonce),
        instructions: Some(
            "Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here.".into(),
        ),
    });
    let manual_abort = interaction.signal.child_token();
    let prompt = AuthPrompt {
        kind: AuthPromptKind::ManualCode,
        message: "Complete login in your browser, or paste the final redirect URL here:".into(),
        placeholder: Some(REDIRECT_URI.into()),
        options: Vec::new(),
        signal: Some(manual_abort.clone()),
    };
    let CallbackServer {
        result: callback_result,
        shutdown,
        listener,
        connections,
    } = callback;
    let outcome = async {
        let manual = async {
            let input = interaction.interaction.prompt(prompt).await?;
            authorization_result_from_manual_input(&input, &state)
        };
        let callback = async {
            match callback_result.await {
                Ok(result) => result.map_err(|message| anyhow::anyhow!(message)),
                Err(_) => anyhow::bail!("OpenAI OAuth callback server closed"),
            }
        };
        let result = tokio::select! {
            result = callback => result?,
            result = manual => result?,
        };
        interaction.interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".into(),
        });
        exchange_authorization_code(
            &fetch,
            &result.code,
            &pkce.verifier,
            &result.client_id,
            &interaction.signal,
        )
        .await
    }
    .await;
    manual_abort.cancel();
    close_server(&shutdown, &listener, &connections);
    match outcome {
        Err(_) if interaction.signal.is_cancelled() => anyhow::bail!("Login cancelled"),
        other => other,
    }
}

/// `openaiChatGPTOAuth`.
pub struct OpenAIChatGPTOAuth;

#[async_trait]
impl OAuthAuth for OpenAIChatGPTOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT subscription)"
    }
    fn is_subscription(&self) -> Option<bool> {
        Some(true)
    }
    fn login_label(&self) -> Option<&str> {
        Some("Sign in with ChatGPT")
    }
    async fn login(
        &self,
        interaction: ProviderAuthInteraction,
        options: Option<LoginOptions>,
    ) -> anyhow::Result<OAuthCredential> {
        login_openai_chatgpt(interaction, options).await
    }
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> anyhow::Result<OAuthCredential> {
        refresh_access_token(&current_oauth_fetch(), &credential, &signal).await
    }
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

pub fn openai_chatgpt_oauth() -> Arc<dyn OAuthAuth> {
    Arc::new(OpenAIChatGPTOAuth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_input_validation() {
        assert_eq!(
            authorization_result_from_manual_input("not a url", "s")
                .unwrap_err()
                .to_string(),
            "Paste the full callback URL from the browser"
        );
        assert_eq!(
            authorization_result_from_manual_input(
                "http://localhost:1455/auth/callback?code=c&state=s",
                "s"
            )
            .unwrap_err()
            .to_string(),
            format!("The pasted callback URL must start with {REDIRECT_URI}")
        );
        let ok = authorization_result_from_manual_input(
            &format!("{REDIRECT_URI}?code=c&state=s&client_id=%20cid%20"),
            "s",
        )
        .unwrap();
        assert_eq!(
            ok,
            AuthorizationResult {
                code: "c".into(),
                client_id: "cid".into()
            }
        );
        assert_eq!(
            authorization_result_from_manual_input(
                &format!("{REDIRECT_URI}?code=c&state=x&client_id=cid"),
                "s"
            )
            .unwrap_err()
            .to_string(),
            "OAuth state mismatch"
        );
    }

    #[test]
    fn host_ids() {
        assert_eq!(
            agent_host_id(Some("0189ABCD-0000-7000-8000-000000000000".into())).unwrap(),
            "urn:uuid:0189abcd-0000-7000-8000-000000000000"
        );
        assert!(agent_host_id(None).is_err());
    }
}
