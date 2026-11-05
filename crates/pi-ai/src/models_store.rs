//! Persistent model catalogs (port of `models-store.ts`).

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::AnyModel;
use crate::utils::abort::SignalAbortError;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    /// Persisted models of every type.
    pub models: Vec<AnyModel>,
    /// Unix timestamp from the remote catalog's Last-Modified header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,
    /// Unix timestamp of the last completed remote check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// Opaque ETag validator, stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl ModelsStoreEntry {
    /// Parse an entry, dropping models whose type this version does not know.
    pub fn from_value_lenient(value: Value) -> anyhow::Result<Self> {
        let mut value = value;
        if let Some(models) = value.get_mut("models").and_then(Value::as_array_mut) {
            models.retain(|m| matches!(AnyModel::raw_type(m), Some("chat" | "image" | "classifier")));
        }
        Ok(serde_json::from_value(value)?)
    }
}

/// Persistent model catalogs keyed by provider ID.
#[async_trait]
pub trait ModelsStore: Send + Sync {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<ModelsStoreEntry>>;
    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry, signal: Option<&CancellationToken>) -> anyhow::Result<()>;
    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct InMemoryModelsStore {
    entries: Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn check(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(|s| s.is_cancelled()) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

#[async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<ModelsStoreEntry>> {
        check(signal)?;
        Ok(self.entries.lock().get(provider_id).cloned())
    }
    async fn write(&self, provider_id: &str, entry: ModelsStoreEntry, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        check(signal)?;
        self.entries.lock().insert(provider_id.to_string(), entry);
        Ok(())
    }
    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        check(signal)?;
        self.entries.lock().remove(provider_id);
        Ok(())
    }
}
