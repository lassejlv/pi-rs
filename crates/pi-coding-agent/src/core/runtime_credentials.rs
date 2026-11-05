//! Port of `packages/coding-agent/src/core/runtime-credentials.ts`.

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::auth::types::{
    ApiKeyCredential, AuthType, Credential, CredentialInfo, CredentialStore, ModifyCredentialFn,
};
use pi_ai::utils::abort::SignalAbortError;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn throw_if_aborted(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

/// Async credential store overlay for non-persistent runtime API keys.
/// TS: `implements CredentialStore`
pub struct RuntimeCredentials {
    store: Arc<dyn CredentialStore>,
    overrides: Mutex<IndexMap<String, String>>,
}

impl RuntimeCredentials {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self {
            store,
            overrides: Mutex::new(IndexMap::new()),
        }
    }

    /// The wrapped persistent store.
    pub fn store(&self) -> &Arc<dyn CredentialStore> {
        &self.store
    }

    pub fn set_runtime_api_key(&self, provider_id: &str, api_key: &str) {
        self.overrides
            .lock()
            .insert(provider_id.to_string(), api_key.to_string());
    }

    pub fn remove_runtime_api_key(&self, provider_id: &str) {
        self.overrides.lock().shift_remove(provider_id);
    }

    pub fn has_runtime_api_key(&self, provider_id: &str) -> bool {
        self.overrides.lock().contains_key(provider_id)
    }
}

#[async_trait]
impl CredentialStore for RuntimeCredentials {
    async fn read(
        &self,
        provider_id: &str,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        throw_if_aborted(signal)?;
        let override_key = self
            .overrides
            .lock()
            .get(provider_id)
            .filter(|key| !key.is_empty())
            .cloned();
        match override_key {
            Some(key) => Ok(Some(Credential::ApiKey(ApiKeyCredential {
                key: Some(key),
                env: None,
            }))),
            None => self.store.read(provider_id, signal).await,
        }
    }

    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        let mut entries: IndexMap<String, CredentialInfo> = self
            .store
            .list(signal)
            .await?
            .into_iter()
            .map(|entry| (entry.provider_id.clone(), entry))
            .collect();
        throw_if_aborted(signal)?;
        for provider_id in self.overrides.lock().keys() {
            entries.insert(
                provider_id.clone(),
                CredentialInfo {
                    provider_id: provider_id.clone(),
                    credential_type: AuthType::ApiKey,
                },
            );
        }
        Ok(entries.into_values().collect())
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyCredentialFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        self.store.modify(provider_id, f, signal).await
    }

    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        throw_if_aborted(signal)?;
        self.store.delete(provider_id, signal).await?;
        self.overrides.lock().shift_remove(provider_id);
        Ok(())
    }
}
