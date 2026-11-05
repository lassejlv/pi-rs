//! Auth types (port of `auth/types.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::types::{ProviderEnv, ProviderHeaders};

/// Request auth for a single model request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub base_url: Option<String>,
}

/// Stored api-key credential.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// OAuth token data (`OAuthCredentials`): refresh/access/expires plus provider extras.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    /// Expiry as Unix milliseconds.
    #[serde(with = "crate::serde_util::lax_i64")]
    pub expires: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Stored canonical OAuth credential.
pub type OAuthCredential = OAuthCredentials;

/// One type-tagged credential per provider (the shape of `auth.json` entries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    pub fn auth_type(&self) -> AuthType {
        match self {
            Credential::ApiKey(_) => AuthType::ApiKey,
            Credential::OAuth(_) => AuthType::OAuth,
        }
    }
    pub fn as_api_key(&self) -> Option<&ApiKeyCredential> {
        match self {
            Credential::ApiKey(c) => Some(c),
            _ => None,
        }
    }
    pub fn as_oauth(&self) -> Option<&OAuthCredential> {
        match self {
            Credential::OAuth(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

impl AuthType {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::ApiKey => "api_key",
            AuthType::OAuth => "oauth",
        }
    }
}

/// Non-secret credential metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub credential_type: AuthType,
}

/// Mutation callback for [`CredentialStore::modify`].
pub type ModifyCredentialFn =
    Box<dyn FnOnce(Option<Credential>) -> BoxFuture<'static, anyhow::Result<Option<Credential>>> + Send>;

/// App-owned credential storage keyed by provider id.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>>;
    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>>;
    /// Serialized read-modify-write. `f` returns the new credential, or `None` to leave it.
    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyCredentialFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>>;
    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()>;
}

/// Environment access for auth resolution.
#[async_trait]
pub trait AuthContext: Send + Sync {
    async fn env(&self, name: &str) -> Option<String>;
    /// Check whether a file exists. Supports a leading `~`.
    async fn file_exists(&self, path: &str) -> bool;
}

/// Result of resolving auth for a model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    /// Human-readable label for status UI.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub auth_type: AuthType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthSelectOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Prompt shown to the user during login.
#[derive(Debug, Clone)]
pub struct AuthPrompt {
    pub kind: AuthPromptKind,
    pub message: String,
    pub placeholder: Option<String>,
    pub options: Vec<AuthSelectOption>,
    /// Cancels this prompt when an out-of-band event resolves the step.
    pub signal: Option<CancellationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPromptKind {
    Text,
    Secret,
    Select,
    ManualCode,
}

impl AuthPrompt {
    pub fn secret(message: impl Into<String>) -> Self {
        Self { kind: AuthPromptKind::Secret, message: message.into(), placeholder: None, options: Vec::new(), signal: None }
    }
    pub fn text(message: impl Into<String>) -> Self {
        Self { kind: AuthPromptKind::Text, message: message.into(), placeholder: None, options: Vec::new(), signal: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    Info { message: String, links: Vec<AuthInfoLink> },
    AuthUrl { url: String, instructions: Option<String> },
    DeviceCode { user_code: String, verification_uri: String, interval_seconds: Option<u64>, expires_in_seconds: Option<u64> },
    Progress { message: String },
}

/// Login interaction callbacks serving both api-key and OAuth flows.
#[async_trait]
pub trait AuthInteraction: Send + Sync {
    /// Returns the entered/selected string (`select` returns the option id).
    async fn prompt(&self, prompt: AuthPrompt) -> anyhow::Result<String>;
    fn notify(&self, event: AuthEvent);
}

/// Normalized interaction passed to provider login implementations.
#[derive(Clone)]
pub struct ProviderAuthInteraction {
    pub interaction: Arc<dyn AuthInteraction>,
    pub signal: CancellationToken,
}

impl ProviderAuthInteraction {
    pub fn throw_if_aborted(&self) -> anyhow::Result<()> {
        if self.signal.is_cancelled() {
            anyhow::bail!(crate::utils::abort::SignalAbortError);
        }
        Ok(())
    }
}

/// Input of [`ApiKeyAuth::resolve`] and [`ApiKeyAuth::check`].
#[derive(Clone)]
pub struct ApiKeyResolveInput {
    pub ctx: Arc<dyn AuthContext>,
    pub credential: Option<ApiKeyCredential>,
    pub signal: CancellationToken,
}

impl ApiKeyResolveInput {
    pub fn throw_if_aborted(&self) -> anyhow::Result<()> {
        if self.signal.is_cancelled() {
            anyhow::bail!(crate::utils::abort::SignalAbortError);
        }
        Ok(())
    }
}

/// Api-key auth: stored key/provider env plus ambient sources.
#[async_trait]
pub trait ApiKeyAuth: Send + Sync {
    /// Display name, e.g. "Anthropic API key".
    fn name(&self) -> &str;
    /// Whether interactive setup is available (`login` present).
    fn supports_login(&self) -> bool {
        false
    }
    async fn login(&self, _interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        anyhow::bail!("{} does not support login", self.name())
    }
    /// Whether a side-effect-free `check` is implemented.
    fn supports_check(&self) -> bool {
        false
    }
    async fn check(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        Ok(None)
    }
    /// Resolve auth from the stored credential and/or ambient sources. `None` = not configured.
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>>;
}

/// App-supplied context for `Models::login`.
#[derive(Clone, Default)]
pub struct LoginOptions {
    pub get_device_id: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub agent_name: Option<String>,
}

/// OAuth auth.
#[async_trait]
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    fn is_subscription(&self) -> Option<bool> {
        None
    }
    fn login_label(&self) -> Option<&str> {
        None
    }
    async fn login(&self, interaction: ProviderAuthInteraction, options: Option<LoginOptions>) -> anyhow::Result<OAuthCredential>;
    /// Exchange the refresh token.
    async fn refresh(&self, credential: OAuthCredential, signal: CancellationToken) -> anyhow::Result<OAuthCredential>;
    /// Side-effect-free derivation of request auth from a valid credential.
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth>;
}

/// Provider auth. At least one of `api_key`/`oauth` must be present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

impl std::fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderAuth")
            .field("api_key", &self.api_key.as_ref().map(|a| a.name().to_string()))
            .field("oauth", &self.oauth.as_ref().map(|a| a.name().to_string()))
            .finish()
    }
}
