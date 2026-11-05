//! Provider auth resolution (port of `auth/resolve.ts`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthContext, AuthResult, Credential, CredentialStore,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
use crate::types::ProviderEnv;
pub use crate::utils::models_error::{ModelsError, ModelsErrorCode};
use crate::utils::abort::SignalAbortError;

#[derive(Debug, Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// Require this much remaining OAuth-token validity; defaults to five minutes.
    pub min_oauth_validity_ms: Option<i64>,
    pub signal: Option<CancellationToken>,
}

const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: i64 = 5 * 60 * 1000;
const DEFAULT_OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

fn aborted(signal: &CancellationToken) -> anyhow::Result<()> {
    if signal.is_cancelled() {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

/// Race a future with a cancellation signal.
pub async fn race_with_signal<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    signal: &CancellationToken,
) -> anyhow::Result<T> {
    aborted(signal)?;
    // An abort observed in the same poll as the result wins, like the TS race whose abort
    // listener fires synchronously before the operation's resolution is delivered.
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(SignalAbortError.into()),
        result = future => result,
    }
}

/// Auth resolution shared by all operations in a `Models` collection.
pub async fn resolve_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &Arc<dyn CredentialStore>,
    auth_context: &Arc<dyn AuthContext>,
    overrides: &AuthResolutionOverrides,
) -> anyhow::Result<Option<AuthResult>> {
    let signal = overrides.signal.clone().unwrap_or_default();
    race_with_signal(resolve_inner(provider_id, auth, credentials, auth_context, overrides, &signal), &signal).await
}

async fn resolve_inner(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &Arc<dyn CredentialStore>,
    auth_context: &Arc<dyn AuthContext>,
    overrides: &AuthResolutionOverrides,
    signal: &CancellationToken,
) -> anyhow::Result<Option<AuthResult>> {
    aborted(signal)?;
    let request_ctx: Arc<dyn AuthContext> = match &overrides.env {
        Some(env) => Arc::new(OverlayEnvAuthContext { base: auth_context.clone(), env: env.clone() }),
        None => auth_context.clone(),
    };

    if let (Some(api_key), Some(api_key_auth)) = (&overrides.api_key, &auth.api_key) {
        return resolve_api_key(
            request_ctx,
            api_key_auth,
            provider_id,
            Some(ApiKeyCredential { key: Some(api_key.clone()), env: overrides.env.clone() }),
            signal,
        )
        .await;
    }

    let stored = read_credential(credentials, provider_id, signal).await?;
    if let Some(stored) = stored {
        return match (stored, &auth.oauth, &auth.api_key) {
            (Credential::OAuth(oauth_cred), Some(oauth), _) => {
                resolve_stored_oauth(credentials, provider_id, oauth, oauth_cred, signal, overrides.min_oauth_validity_ms).await
            }
            (Credential::ApiKey(mut cred), _, Some(api_key_auth)) => {
                if let Some(env) = &overrides.env {
                    let mut merged = cred.env.clone().unwrap_or_default();
                    merged.extend(env.clone());
                    cred.env = Some(merged);
                }
                resolve_api_key(request_ctx, api_key_auth, provider_id, Some(cred), signal).await
            }
            _ => Ok(None),
        };
    }

    match &auth.api_key {
        Some(api_key_auth) => resolve_api_key(request_ctx, api_key_auth, provider_id, None, signal).await,
        None => Ok(None),
    }
}

struct OverlayEnvAuthContext {
    base: Arc<dyn AuthContext>,
    env: ProviderEnv,
}

#[async_trait]
impl AuthContext for OverlayEnvAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        match self.env.get(name).filter(|v| !v.is_empty()) {
            Some(v) => Some(v.clone()),
            None => self.base.env(name).await,
        }
    }
    async fn file_exists(&self, path: &str) -> bool {
        self.base.file_exists(path).await
    }
}

/// Refresh a stored OAuth credential under the credential-store lock.
pub async fn refresh_stored_oauth_credential(
    credentials: &Arc<dyn CredentialStore>,
    provider_id: &str,
    oauth: &Arc<dyn OAuthAuth>,
    needs_refresh: Arc<dyn Fn(&OAuthCredential) -> bool + Send + Sync>,
    signal: &CancellationToken,
) -> anyhow::Result<Option<OAuthCredential>> {
    let oauth = oauth.clone();
    let provider = provider_id.to_string();
    let outer_signal = signal.clone();
    // `signal` cancels only the wait for the lock; once the refresh starts it is ignored so a
    // rotated refresh token is always persisted.
    let lock_wait = CancellationToken::new();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let signal = signal.clone();
        let lock_wait = lock_wait.clone();
        let started = started.clone();
        crate::utils::wasm::spawn(async move {
            signal.cancelled().await;
            if !started.load(std::sync::atomic::Ordering::SeqCst) {
                lock_wait.cancel();
            }
        })
    };
    let started_in_fn = started.clone();
    // The modification runs in its own task: a caller that stops waiting (and drops this future)
    // must not drop an in-flight refresh, or the rotated refresh token would never be persisted.
    let store = credentials.clone();
    let store_id = provider_id.to_string();
    let lock_signal = lock_wait.clone();
    let operation = crate::utils::wasm::spawn(async move {
        store
            .modify(
                &store_id,
                Box::new(move |current| {
                    Box::pin(async move {
                        started_in_fn.store(true, std::sync::atomic::Ordering::SeqCst);
                        aborted(&outer_signal)?;
                        let Some(Credential::OAuth(current)) = current else { return Ok(None) };
                        if !needs_refresh(&current) {
                            return Ok(None);
                        }
                        let timeout = CancellationToken::new();
                        let timer = timeout.clone();
                        let handle = crate::utils::wasm::spawn(async move {
                            crate::utils::wasm::sleep(Duration::from_millis(DEFAULT_OAUTH_REFRESH_TIMEOUT_MS)).await;
                            timer.cancel();
                        });
                        let refreshed = oauth.refresh(current, timeout).await;
                        handle.abort();
                        match refreshed {
                            Ok(credential) => Ok(Some(Credential::OAuth(credential))),
                            Err(error) => Err(ModelsError::with_cause(
                                ModelsErrorCode::OAuth,
                                format!("OAuth refresh failed for {provider}"),
                                &error,
                            )
                            .into()),
                        }
                    })
                }),
                Some(&lock_signal),
            )
            .await
    });
    let result = operation.await.unwrap_or_else(|error| Err(anyhow::anyhow!("credential refresh task failed: {error}")));
    watcher.abort();
    match result {
        Ok(post) => Ok(match post {
            Some(Credential::OAuth(c)) => Some(c),
            _ => None,
        }),
        Err(error) => {
            if error.downcast_ref::<ModelsError>().is_some() {
                return Err(error);
            }
            aborted(signal)?;
            Err(ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("Credential store modify failed for {provider_id}"),
                &error,
            )
            .into())
        }
    }
}

async fn resolve_stored_oauth(
    credentials: &Arc<dyn CredentialStore>,
    provider_id: &str,
    oauth: &Arc<dyn OAuthAuth>,
    stored: OAuthCredential,
    signal: &CancellationToken,
    min_oauth_validity_ms: Option<i64>,
) -> anyhow::Result<Option<AuthResult>> {
    let minimum = DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_oauth_validity_ms.unwrap_or(0));
    let expires_soon = Arc::new(move |c: &OAuthCredential| crate::utils::now_ms() + minimum >= c.expires);
    let mut credential = stored;
    if expires_soon(&credential) {
        let post = refresh_stored_oauth_credential(credentials, provider_id, oauth, expires_soon.clone(), signal).await?;
        let Some(post) = post else { return Ok(None) };
        credential = post;
        if min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(ModelsError::new(
                ModelsErrorCode::OAuth,
                format!("OAuth refresh returned a token that expires too soon for {provider_id}"),
            )
            .into());
        }
    }
    match oauth.to_auth(&credential).await {
        Ok(auth) => Ok(Some(AuthResult { auth, env: None, source: Some("OAuth".to_string()) })),
        Err(error) => Err(ModelsError::with_cause(
            ModelsErrorCode::OAuth,
            format!("OAuth auth derivation failed for {provider_id}"),
            &error,
        )
        .into()),
    }
}

async fn resolve_api_key(
    ctx: Arc<dyn AuthContext>,
    api_key: &Arc<dyn ApiKeyAuth>,
    provider_id: &str,
    credential: Option<ApiKeyCredential>,
    signal: &CancellationToken,
) -> anyhow::Result<Option<AuthResult>> {
    api_key.resolve(ApiKeyResolveInput { ctx, credential, signal: signal.clone() }).await.map_err(|error| {
        ModelsError::with_cause(ModelsErrorCode::Auth, format!("API key auth failed for provider {provider_id}"), &error)
            .into()
    })
}

async fn read_credential(
    credentials: &Arc<dyn CredentialStore>,
    provider_id: &str,
    signal: &CancellationToken,
) -> anyhow::Result<Option<Credential>> {
    credentials.read(provider_id, Some(signal)).await.map_err(|error| {
        ModelsError::with_cause(ModelsErrorCode::Auth, format!("Credential store read failed for {provider_id}"), &error)
            .into()
    })
}
