//! Port of `packages/coding-agent/src/core/models-store.ts`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::models_store::{ModelsStore, ModelsStoreEntry};
use pi_ai::utils::abort::race_with_abort_signal;
use pi_ai::utils::abort::SignalAbortError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::get_agent_dir;
use crate::core::auth_storage::{
    AuthStorageBackend, AuthStorageBackendExt, FileAuthStorageBackend, LockResult,
};
use crate::utils::paths::{get_file_revision, normalize_path};
use crate::utils::text::strip_bom;

/// Raw models-store.json content. Entries are parsed on read so entries written by other pi
/// versions (unknown model types) are preserved on rewrite.
pub type StoredModels = IndexMap<String, Value>;

fn throw_if_aborted(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

/// TS: `implements ModelsStore`
#[derive(Default)]
pub struct InMemoryCodingAgentModelsStore {
    entries: Mutex<IndexMap<String, ModelsStoreEntry>>,
}

impl InMemoryCodingAgentModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModelsStore for InMemoryCodingAgentModelsStore {
    async fn read(
        &self,
        provider_id: &str,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<ModelsStoreEntry>> {
        throw_if_aborted(signal)?;
        Ok(self.entries.lock().get(provider_id).cloned())
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<()> {
        throw_if_aborted(signal)?;
        self.entries.lock().insert(provider_id.to_string(), entry);
        Ok(())
    }

    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        throw_if_aborted(signal)?;
        self.entries.lock().shift_remove(provider_id);
        Ok(())
    }
}

// Readers share the parsed file: each `read` clones only its provider's entry, not every provider's.
type ReloadFuture = Shared<BoxFuture<'static, Result<Arc<StoredModels>, Arc<anyhow::Error>>>>;

struct ModelsFileReload {
    id: u64,
    controller: CancellationToken,
    promise: ReloadFuture,
    readers: usize,
}

#[derive(Default)]
struct ModelsFileReadState {
    data: Arc<StoredModels>,
    revision: Option<String>,
    reload: Option<ModelsFileReload>,
}

type SharedReadState = Arc<Mutex<ModelsFileReadState>>;

// Optimize the common path without retaining an unbounded set of custom paths.
static SHARED_MODELS_FILE_READ_STATE: Mutex<Option<(String, SharedReadState)>> = Mutex::new(None);
static NEXT_RELOAD_ID: AtomicU64 = AtomicU64::new(1);

fn parse(content: Option<&str>) -> anyhow::Result<StoredModels> {
    match content {
        Some(content) if !content.is_empty() => {
            match serde_json::from_str::<Value>(&strip_bom(content))? {
                Value::Object(map) => Ok(map.into_iter().collect()),
                Value::Null => Ok(StoredModels::new()),
                _ => anyhow::bail!("Invalid models store: expected an object"),
            }
        }
        _ => Ok(StoredModels::new()),
    }
}

/// Locked JSON-backed storage for dynamically refreshed provider catalogs.
/// TS: `implements ModelsStore`
pub struct FileModelsStore {
    storage: Arc<dyn AuthStorageBackend>,
    path: String,
    read_state: SharedReadState,
}

impl FileModelsStore {
    pub fn new(path: Option<&str>) -> Self {
        let path = path.map(str::to_string).unwrap_or_else(|| {
            Path::new(&get_agent_dir())
                .join("models-store.json")
                .to_string_lossy()
                .into_owned()
        });
        let path = normalize_path(&path, None);
        let read_state = {
            let mut shared = SHARED_MODELS_FILE_READ_STATE.lock();
            match shared.as_ref() {
                Some((shared_path, state)) if *shared_path == path => state.clone(),
                Some(_) => Arc::default(),
                None => {
                    let state: SharedReadState = Arc::default();
                    *shared = Some((path.clone(), state.clone()));
                    state
                }
            }
        };
        Self {
            storage: Arc::new(FileAuthStorageBackend::new(Some(&path))),
            path,
            read_state,
        }
    }

    fn update_read_state(state: &SharedReadState, data: Arc<StoredModels>, revision: Option<String>) {
        let mut state = state.lock();
        state.data = data;
        state.revision = revision;
    }

    fn reload_from_storage(&self, signal: CancellationToken) -> ReloadFuture {
        let storage = self.storage.clone();
        let path = self.path.clone();
        let read_state = self.read_state.clone();
        async move {
            storage
                .with_lock_async(
                    move |content| async move {
                        let data = Arc::new(parse(content.as_deref())?);
                        FileModelsStore::update_read_state(
                            &read_state,
                            data.clone(),
                            get_file_revision(&path),
                        );
                        Ok(LockResult::read(data))
                    },
                    Some(&signal),
                )
                .await
                .map_err(Arc::new)
        }
        .boxed()
        .shared()
    }

    async fn read_latest(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Arc<StoredModels>> {
        throw_if_aborted(signal)?;
        let revision = get_file_revision(&self.path);
        let (id, promise) = {
            let mut state = self.read_state.lock();
            if revision.is_some() && revision == state.revision {
                return Ok(state.data.clone());
            }
            if state.reload.is_none() {
                let controller = CancellationToken::new();
                let promise = self.reload_from_storage(controller.clone());
                state.reload = Some(ModelsFileReload {
                    id: NEXT_RELOAD_ID.fetch_add(1, Ordering::Relaxed),
                    controller,
                    promise,
                    readers: 0,
                });
            }
            let reload = state.reload.as_mut().expect("reload in flight");
            reload.readers += 1;
            (reload.id, reload.promise.clone())
        };

        let operation = async { promise.await.map_err(|error| anyhow::anyhow!("{error:#}")) };
        let result = match signal {
            Some(signal) => race_with_abort_signal(operation, signal).await,
            None => operation.await,
        };

        let mut state = self.read_state.lock();
        if let Some(reload) = state.reload.as_mut()
            && reload.id == id
        {
            reload.readers -= 1;
            let settled = reload.promise.peek().is_some();
            if reload.readers == 0 || settled {
                let reload = state.reload.take().expect("reload in flight");
                if !settled {
                    reload.controller.cancel();
                }
            }
        }
        result
    }
}

#[async_trait]
impl ModelsStore for FileModelsStore {
    async fn read(
        &self,
        provider_id: &str,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<Option<ModelsStoreEntry>> {
        let data = self.read_latest(signal).await?;
        throw_if_aborted(signal)?;
        match data.get(provider_id) {
            Some(entry) => Ok(Some(ModelsStoreEntry::from_value_lenient(entry.clone())?)),
            None => Ok(None),
        }
    }

    async fn write(
        &self,
        provider_id: &str,
        entry: ModelsStoreEntry,
        signal: Option<&CancellationToken>,
    ) -> anyhow::Result<()> {
        let provider_id = provider_id.to_string();
        let value = serde_json::to_value(&entry)?;
        let latest = self
            .storage
            .with_lock_async(
                move |content| async move {
                    let mut current = parse(content.as_deref())?;
                    current.insert(provider_id, value);
                    let next = serde_json::to_string_pretty(&current)?;
                    Ok(LockResult::write(current, next))
                },
                signal,
            )
            .await?;
        Self::update_read_state(&self.read_state, Arc::new(latest), None);
        Ok(())
    }

    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        let provider_id = provider_id.to_string();
        let latest = self
            .storage
            .with_lock_async(
                move |content| async move {
                    let mut current = parse(content.as_deref())?;
                    current.shift_remove(&provider_id);
                    let next = serde_json::to_string_pretty(&current)?;
                    Ok(LockResult::write(current, next))
                },
                signal,
            )
            .await?;
        Self::update_read_state(&self.read_state, Arc::new(latest), None);
        Ok(())
    }
}
