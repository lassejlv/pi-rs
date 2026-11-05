//! In-memory credential store (port of `auth/credential-store.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::types::{Credential, CredentialInfo, CredentialStore, ModifyCredentialFn};
use crate::utils::abort::SignalAbortError;

/// Default in-memory credential store. Writes are serialized per provider.
#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<indexmap::IndexMap<String, Credential>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_for(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().entry(provider_id.to_string()).or_default().clone()
    }
}

fn check(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(|s| s.is_cancelled()) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

async fn acquire(
    lock: &Arc<tokio::sync::Mutex<()>>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<tokio::sync::OwnedMutexGuard<()>> {
    check(signal)?;
    match signal {
        Some(signal) => tokio::select! {
            guard = lock.clone().lock_owned() => Ok(guard),
            _ = signal.cancelled() => Err(SignalAbortError.into()),
        },
        None => Ok(lock.clone().lock_owned().await),
    }
}

#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        check(signal)?;
        Ok(self.credentials.lock().get(provider_id).cloned())
    }

    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        check(signal)?;
        Ok(self
            .credentials
            .lock()
            .iter()
            .map(|(id, c)| CredentialInfo { provider_id: id.clone(), credential_type: c.auth_type() })
            .collect())
    }

    async fn modify(
        &self,
        provider_id: &str,
        f: ModifyCredentialFn,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<Credential>> {
        let lock = self.lock_for(provider_id);
        let _guard = acquire(&lock, signal).await?;
        let current = self.credentials.lock().get(provider_id).cloned();
        let next = f(current.clone()).await?;
        check(signal)?;
        match next {
            Some(next) => {
                self.credentials.lock().insert(provider_id.to_string(), next.clone());
                Ok(Some(next))
            }
            None => Ok(current),
        }
    }

    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        let lock = self.lock_for(provider_id);
        let _guard = acquire(&lock, signal).await?;
        self.credentials.lock().shift_remove(provider_id);
        Ok(())
    }
}
