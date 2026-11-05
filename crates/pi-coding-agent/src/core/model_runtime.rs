//! Port of `packages/coding-agent/src/core/model-runtime.ts`.
//!
//! Configured pi-ai `Models` collection used by coding-agent and SDK consumers: built-in providers
//! (with pi.dev catalog overlays), models.json overlays, extension providers and virtual models,
//! plus a synchronous snapshot of model availability.

use crate::compat::AuthOperationOptions;
use crate::config::get_agent_dir;
use crate::core::auth_storage::AuthStorage;
use crate::core::model_config::ModelConfig;
use crate::core::models_store::{FileModelsStore, InMemoryCodingAgentModelsStore};
use crate::core::provider_composer::{
    AuthStatus, AuthStatusSource, CompatibilityRequestConfig, ProviderConfigInput,
    compose_model_provider, configured_request_auth_status, resolve_compatibility_request_config,
    resolve_configured_model_headers, validate_extension_provider,
};
use crate::core::remote_catalog_provider::{
    PersonalizeCatalog, with_personalized_catalog, with_remote_catalog,
};
use crate::core::runtime_credentials::RuntimeCredentials;
use crate::core::virtual_models::{
    ModelRoute, ModelRouteReason, ModelRouteRequest, ModelRouteRequestFailed,
    ModelRouteRequestPrevious, VirtualModelDefinition, VirtualModelRouteFn, create_virtual_model,
    find_latest_llm_response, is_virtual_model, with_virtual_models,
};
use futures::FutureExt;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use pi_ai::api::lazy::lazy_stream;
use pi_ai::auth::resolve::AuthResolutionOverrides;
use pi_ai::auth::types::{
    ApiKeyCredential, AuthCheck, AuthInteraction, AuthResult, AuthType, Credential,
    CredentialInfo, CredentialStore, LoginOptions,
};
use pi_ai::models::{
    CreateModelsOptions, Models, ModelsError, ModelsErrorCode, ModelsRefreshOptions,
    ModelsRefreshResult, ModelsRequestOptions, Provider, clamp_thinking_level, create_models,
    merge_headers,
};
use pi_ai::models_store::ModelsStore;
use pi_ai::providers::model_schema::ModelThinkingLevel;
use pi_ai::types::{
    AnyModel, AssistantImages, AssistantMessage, AssistantMessageEventStream, ClassifierContext,
    ClassifierModel, ClassifierResult, Context, DeferredHandle, ImageModel, ImagesContext,
    Message, Model, ModelType, ProviderHeaders, StreamOptions,
};
use pi_ai::utils::identity::{ClientIdentityTarget, get_open_code_session_headers};
use pi_ai::utils::model_operations::{
    assert_chat_model, assert_classifier_input_supported, assert_classifier_model,
    assert_image_model, classifier_error_result, image_error_result,
};
use pi_ai::utils::abort::SignalAbortError;
use pi_ai::utils::transcript::normalize_context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

/// A virtual model registered with the runtime.
#[derive(Clone)]
pub struct RegisteredVirtualModel {
    pub model: Model,
    pub route: VirtualModelRouteFn,
}

/// Synchronous model/auth snapshot, refreshed by availability passes.
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeSnapshot {
    pub all: Vec<Model>,
    pub available: Vec<Model>,
    pub configured_providers: IndexSet<String>,
    pub stored_providers: IndexSet<String>,
    pub auth: IndexMap<String, AuthCheck>,
}

#[derive(Clone, Default)]
pub struct CreateModelRuntimeOptions {
    /// Credential storage. Defaults to the file at authPath.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub auth_path: Option<String>,
    /// models.json path. Defaults to `<agentDir>/models.json`; an empty string disables
    /// models.json and the file-backed models store (TS `modelsPath: null`).
    pub models_path: Option<String>,
    pub models_store: Option<Arc<dyn ModelsStore>>,
    pub models_store_path: Option<String>,
    /// Allow create() to refresh model catalogs over the network. Defaults to false.
    pub allow_model_network: Option<bool>,
    /// Timeout for the create-time network model refresh.
    pub model_refresh_timeout_ms: Option<i64>,
    pub catalog_base_url: Option<String>,
    /// Optional caller cancellation for initial cache restoration and availability checks.
    pub signal: Option<CancellationToken>,
    /// Skip initial catalog and availability refresh. Static models remain available.
    pub refresh_on_create: Option<bool>,
}

// TS: extends AuthOperationOptions (inherited fields inlined)
#[derive(Debug, Clone, Default)]
pub struct ModelRuntimeAuthOverrides {
    pub signal: Option<CancellationToken>,
    pub api_key: Option<String>,
    pub env: Option<IndexMap<String, String>>,
    /// Require this much remaining OAuth-token validity; defaults to five minutes.
    pub min_o_auth_validity_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CredentialSynchronizationOperation {
    #[serde(rename = "login")]
    Login,
    #[serde(rename = "logout")]
    Logout,
    #[serde(rename = "setRuntimeApiKey")]
    SetRuntimeApiKey,
    #[serde(rename = "removeRuntimeApiKey")]
    RemoveRuntimeApiKey,
}

impl CredentialSynchronizationOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Logout => "logout",
            Self::SetRuntimeApiKey => "setRuntimeApiKey",
            Self::RemoveRuntimeApiKey => "removeRuntimeApiKey",
        }
    }
}

/// Credentials changed successfully, but the local model/auth snapshot could not be synchronized.
/// TS: `extends Error`
#[derive(Debug, Clone)]
pub struct CredentialSynchronizationError {
    pub message: String,
    provider_id: String,
    operation: CredentialSynchronizationOperation,
    credential: Option<Credential>,
    cause: Arc<anyhow::Error>,
}

impl std::fmt::Display for CredentialSynchronizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CredentialSynchronizationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref().as_ref())
    }
}

impl CredentialSynchronizationError {
    pub fn new(
        provider_id: &str,
        operation: CredentialSynchronizationOperation,
        credential: Option<Credential>,
        cause: anyhow::Error,
    ) -> Self {
        Self {
            message: format!(
                "Credential {} committed for {provider_id}, but local synchronization failed",
                operation.as_str()
            ),
            provider_id: provider_id.to_string(),
            operation,
            credential,
            cause: Arc::new(cause),
        }
    }

    /// Always "CredentialSynchronizationError".
    pub fn name(&self) -> &'static str {
        "CredentialSynchronizationError"
    }

    pub fn provider_id(&self) -> String {
        self.provider_id.clone()
    }

    pub fn operation(&self) -> CredentialSynchronizationOperation {
        self.operation
    }

    pub fn credential(&self) -> Option<Credential> {
        self.credential.clone()
    }

    pub fn cause(&self) -> &anyhow::Error {
        &self.cause
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ModelRuntimeGetAuthProviderOrModel {
    String(String),
    AnyModel(AnyModel),
}

impl From<&str> for ModelRuntimeGetAuthProviderOrModel {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<&Model> for ModelRuntimeGetAuthProviderOrModel {
    fn from(value: &Model) -> Self {
        Self::AnyModel(AnyModel::Chat(value.clone()))
    }
}

#[derive(Debug, Clone)]
pub struct ModelRuntimeResolveModelOptions {
    pub reason: ModelRouteReason,
    pub thinking_level: ModelThinkingLevel,
    pub signal: Option<CancellationToken>,
    pub failed: Option<AssistantMessage>,
    pub state: Option<Value>,
}

fn throw_if_aborted(signal: &CancellationToken) -> anyhow::Result<()> {
    if signal.is_cancelled() {
        return Err(SignalAbortError.into());
    }
    Ok(())
}

fn error_message(error: &anyhow::Error) -> String {
    error.to_string()
}

fn to_provider_env(env: &IndexMap<String, String>) -> pi_ai::types::ProviderEnv {
    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn spawn_detached(future: futures::future::BoxFuture<'static, ()>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        crate::utils::uncaught::spawn_on(&handle, future);
    }
}

/// Source of `RuntimeState::extension_provider_revisions`.
static PROVIDER_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Mutable runtime state. Never held across an await or a call into a provider.
struct RuntimeState {
    builtins: IndexMap<String, Arc<dyn Provider>>,
    native_extension_providers: IndexMap<String, Arc<dyn Provider>>,
    extension_providers: IndexMap<String, ProviderConfigInput>,
    /// Registration revision of each entry of `extension_providers`: TS hands out the stored config
    /// object itself, whose identity changes with every registration.
    extension_provider_revisions: HashMap<String, u64>,
    /// Virtual models by provider id, then model id.
    virtual_models: IndexMap<String, IndexMap<String, RegisteredVirtualModel>>,
    composition_errors: IndexMap<String, String>,
    config: Arc<ModelConfig>,
    snapshot: ModelRuntimeSnapshot,
    availability_refresh_seq: u64,
    availability_error_seq: u64,
    provider_availability_seq: HashMap<String, u64>,
    availability_error: Option<String>,
}

/// Configured pi-ai Models collection used by coding-agent and SDK consumers.
/// TS: `implements Models`
pub struct ModelRuntime {
    weak: Weak<ModelRuntime>,
    models: Models,
    credentials: Arc<RuntimeCredentials>,
    default_builtins: IndexMap<String, Arc<dyn Provider>>,
    models_path: Option<String>,
    model_network_enabled: bool,
    state: Mutex<RuntimeState>,
    credential_operations: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Serializes `refresh()` calls. TS runs them interleaved on one thread; here a refresh that
    /// rebuilds providers must not run concurrently with another one's availability pass.
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    /// A background offline refresh is queued but has not started its pass yet. Further requests
    /// coalesce into it. TS starts `void this.refresh()` immediately at registration; the queued pass
    /// takes the refresh lock at request time so it runs before refreshes requested after it.
    background_refresh_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes provider (re)composition: `rebuild_providers` deletes providers missing from its
    /// snapshot of the registrations, so a registration on another thread must not land between
    /// that snapshot and the deletes (TS runs both on one thread).
    composition: Mutex<()>,
}

/// Request prepared by `prepare_request`.
struct PreparedRequest {
    provider: Arc<dyn Provider>,
    model: AnyModel,
    options: StreamOptions,
}

impl ModelRuntime {
    pub fn new(
        credentials: Arc<RuntimeCredentials>,
        config: Arc<ModelConfig>,
        models_path: Option<&str>,
        models_store: Arc<dyn ModelsStore>,
        providers: Vec<Arc<dyn Provider>>,
        model_network_enabled: bool,
    ) -> Arc<Self> {
        let default_builtins: IndexMap<String, Arc<dyn Provider>> = providers
            .into_iter()
            .map(|provider| (provider.id().to_string(), provider))
            .collect();
        let models = create_models(CreateModelsOptions {
            credentials: Some(credentials.clone() as Arc<dyn CredentialStore>),
            models_store: Some(models_store),
            auth_context: None,
            identity: None,
        });
        let runtime = Arc::new_cyclic(|weak| ModelRuntime {
            weak: weak.clone(),
            models,
            credentials,
            models_path: models_path.map(str::to_string),
            model_network_enabled,
            state: Mutex::new(RuntimeState {
                builtins: default_builtins.clone(),
                native_extension_providers: IndexMap::new(),
                extension_providers: IndexMap::new(),
                extension_provider_revisions: HashMap::new(),
                virtual_models: IndexMap::new(),
                composition_errors: IndexMap::new(),
                config,
                snapshot: ModelRuntimeSnapshot::default(),
                availability_refresh_seq: 0,
                availability_error_seq: 0,
                provider_availability_seq: HashMap::new(),
                availability_error: None,
            }),
            default_builtins,
            credential_operations: Mutex::new(HashMap::new()),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            background_refresh_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            composition: Mutex::new(()),
        });
        runtime.rebuild_providers();
        runtime
    }

    pub async fn create(options: Option<&CreateModelRuntimeOptions>) -> anyhow::Result<Arc<ModelRuntime>> {
        let options = options.cloned().unwrap_or_default();
        let store: Arc<dyn CredentialStore> = match options.credentials.clone() {
            Some(store) => store,
            None => AuthStorage::create(options.auth_path.as_deref()),
        };
        let credentials = Arc::new(RuntimeCredentials::new(store));
        let models_path: Option<String> = match &options.models_path {
            Some(path) if path.is_empty() => None,
            Some(path) => Some(path.clone()),
            None => Some(Path::new(&get_agent_dir()).join("models.json").to_string_lossy().into_owned()),
        };
        let config = ModelConfig::load(models_path.as_deref()).await;
        let models_store: Arc<dyn ModelsStore> = match (&options.models_store, &models_path) {
            (Some(store), _) => store.clone(),
            (None, Some(models_path)) => {
                let store_path = options.models_store_path.clone().unwrap_or_else(|| {
                    Path::new(models_path)
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .join("models-store.json")
                        .to_string_lossy()
                        .into_owned()
                });
                Arc::new(FileModelsStore::new(Some(&store_path)))
            }
            (None, None) => Arc::new(InMemoryCodingAgentModelsStore::new()),
        };
        let generated_at = pi_ai::providers::all::get_builtin_model_data_generated_at();
        let personalize: PersonalizeCatalog = Arc::new(|catalog, context| {
            async move { pi_ai::providers::openrouter::personalize_open_router_catalog(catalog, &context).await }
                .boxed()
        });
        let providers: Vec<Arc<dyn Provider>> = pi_ai::providers::all::builtin_providers()
            .into_iter()
            .map(|provider| match provider.id() {
                "radius" => provider,
                // OpenRouter narrows the catalog to the models, limits and prices of the user's key.
                "openrouter" => with_personalized_catalog(
                    provider,
                    &personalize,
                    options.catalog_base_url.as_deref(),
                    generated_at,
                ),
                _ => with_remote_catalog(provider, options.catalog_base_url.as_deref(), generated_at),
            })
            .collect();
        let runtime = ModelRuntime::new(
            credentials,
            config,
            models_path.as_deref(),
            models_store,
            providers,
            std::env::var_os("PI_OFFLINE").is_none(),
        );
        runtime.configure_radius_providers();
        runtime.rebuild_providers();
        let refresh_from_network = runtime.model_network_enabled && options.allow_model_network == Some(true);
        let timeout = options
            .model_refresh_timeout_ms
            .filter(|_| refresh_from_network)
            .map(|ms| std::time::Duration::from_millis(ms.max(0) as u64));
        let signal = match timeout {
            Some(timeout) => {
                let combined = CancellationToken::new();
                let target = combined.clone();
                let caller = options.signal.clone();
                let watcher = crate::utils::uncaught::spawn(async move {
                    let caller_cancelled = async {
                        match caller {
                            Some(caller) => caller.cancelled().await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(timeout) => target.cancel(),
                        _ = caller_cancelled => target.cancel(),
                        _ = target.cancelled() => {}
                    }
                });
                Some((combined, watcher))
            }
            None => None,
        };
        if options.refresh_on_create != Some(false) {
            let refresh_signal = signal.as_ref().map(|(s, _)| s.clone()).or(options.signal.clone());
            let _ = runtime
                .refresh(Some(ModelsRefreshOptions {
                    allow_network: Some(refresh_from_network),
                    providers: None,
                    force: None,
                    signal: refresh_signal,
                }))
                .await;
        }
        if let Some((_, watcher)) = signal {
            watcher.abort();
        }
        Ok(runtime)
    }

    fn configure_radius_providers(&self) {
        let config = self.state.lock().config.clone();
        let mut builtins = self.default_builtins.clone();
        for provider_id in config.get_provider_ids() {
            let Some(provider) = config.get_provider(&provider_id) else { continue };
            if provider.oauth.as_deref() != Some("radius") {
                continue;
            }
            let Some(base_url) = provider.base_url.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            let gateway = base_url
                .strip_suffix("/v1/")
                .or_else(|| base_url.strip_suffix("/v1"))
                .unwrap_or(base_url)
                .to_string();
            builtins.insert(
                provider_id.clone(),
                pi_ai::providers::radius::radius_provider(pi_ai::providers::radius::RadiusProviderOptions {
                    id: Some(provider_id.clone()),
                    name: Some(provider.name.clone().unwrap_or_else(|| provider_id.clone())),
                    gateway: Some(gateway),
                }),
            );
        }
        self.state.lock().builtins = builtins;
    }

    fn provider_ids(&self) -> IndexSet<String> {
        let state = self.state.lock();
        let mut ids: IndexSet<String> = IndexSet::new();
        ids.extend(state.builtins.keys().cloned());
        ids.extend(state.native_extension_providers.keys().cloned());
        ids.extend(state.config.get_provider_ids());
        ids.extend(state.extension_providers.keys().cloned());
        ids.extend(state.virtual_models.keys().cloned());
        ids
    }

    /// Returns the provider without virtual models, or None when only virtual models define it.
    fn recompose_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        let provider = self.compose_provider(provider_id);
        let virtual_models: Vec<Model> = self
            .state
            .lock()
            .virtual_models
            .get(provider_id)
            .map(|models| models.values().map(|entry| entry.model.clone()).collect())
            .unwrap_or_default();
        if !virtual_models.is_empty() {
            self.models
                .set_provider(with_virtual_models(provider_id, provider.clone(), &virtual_models));
        } else if let Some(provider) = &provider {
            self.models.set_provider(provider.clone());
        } else {
            self.models.delete_provider(provider_id);
        }
        provider
    }

    /// The provider without virtual models, or None when nothing defines it.
    fn compose_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        let (base, extension, config) = {
            let state = self.state.lock();
            (
                state
                    .native_extension_providers
                    .get(provider_id)
                    .or_else(|| state.builtins.get(provider_id))
                    .cloned(),
                state.extension_providers.get(provider_id).cloned(),
                state.config.clone(),
            )
        };
        if config.get_provider(provider_id).is_none() && extension.is_none() {
            // No overlays: use the builtin untouched so its auth/login/stream behavior is exact.
            self.state.lock().composition_errors.shift_remove(provider_id);
            return base;
        }
        match compose_model_provider(provider_id, base.clone(), config, extension.as_ref()) {
            Ok(provider) => {
                self.state.lock().composition_errors.shift_remove(provider_id);
                Some(provider)
            }
            Err(error) => {
                self.state
                    .lock()
                    .composition_errors
                    .insert(provider_id.to_string(), error_message(&error));
                base
            }
        }
    }

    /// Recompose every provider. Providers are replaced one by one (and stale ones removed) rather
    /// than cleared first, so concurrent readers never observe an empty collection.
    fn rebuild_providers(&self) {
        {
            let _composition = self.composition.lock();
            self.state.lock().composition_errors.clear();
            let ids = self.provider_ids();
            for provider_id in &ids {
                self.recompose_provider(provider_id);
            }
            for provider in self.models.get_providers() {
                if !ids.contains(provider.id()) {
                    self.models.delete_provider(provider.id());
                }
            }
        }
        // Outside the composition lock, like every other recomposition: the snapshot reads each
        // provider's models, and an extension's provider answers on its JS thread, which may be
        // waiting for this lock in `registerProvider` (a refresh running beside an extension that
        // registers providers deadlocked here).
        self.update_model_snapshot();
    }

    fn update_model_snapshot(&self) {
        let all = self.models.get_models(None);
        let mut state = self.state.lock();
        let available = all
            .iter()
            .filter(|model| state.snapshot.configured_providers.contains(&model.provider))
            .cloned()
            .collect();
        state.snapshot.all = all;
        state.snapshot.available = available;
    }

    async fn run_availability_refresh(
        &self,
        seq: u64,
        error_seq: u64,
        signal: &CancellationToken,
    ) -> anyhow::Result<()> {
        let providers = self.models.get_providers();
        let checks = futures::future::try_join_all(providers.iter().map(|provider| {
            let id = provider.id().to_string();
            async move {
                let check = self.models.check_auth(&id, Some(signal.clone())).await?;
                anyhow::Ok((id, check))
            }
        }));
        let (available, checks, credentials) = futures::future::try_join3(
            self.models.get_available(None, Some(signal.clone())),
            checks,
            self.credentials.list(Some(signal)),
        )
        .await?;
        let all = self.models.get_models(None);
        let mut state = self.state.lock();
        if seq != state.availability_refresh_seq {
            return Ok(());
        }
        let mut auth = IndexMap::new();
        let mut configured_providers = IndexSet::new();
        for (id, check) in checks {
            if let Some(check) = check {
                configured_providers.insert(id.clone());
                auth.insert(id, check);
            }
        }
        state.snapshot = ModelRuntimeSnapshot {
            all,
            available,
            configured_providers,
            stored_providers: credentials.into_iter().map(|entry| entry.provider_id).collect(),
            auth,
        };
        if error_seq == state.availability_error_seq {
            state.availability_error = None;
        }
        Ok(())
    }

    async fn queue_availability_refresh(&self, signal: Option<CancellationToken>) -> anyhow::Result<()> {
        let (seq, error_seq) = {
            let mut state = self.state.lock();
            state.availability_refresh_seq += 1;
            for value in state.provider_availability_seq.values_mut() {
                *value += 1;
            }
            state.availability_error_seq += 1;
            (state.availability_refresh_seq, state.availability_error_seq)
        };
        let effective_signal = signal.unwrap_or_default();
        let result = self.run_availability_refresh(seq, error_seq, &effective_signal).await;
        if let Err(error) = &result {
            let mut state = self.state.lock();
            if error_seq == state.availability_error_seq && !effective_signal.is_cancelled() {
                state.availability_error = Some(error_message(error));
            }
        }
        result
    }

    async fn refresh_provider_availability(&self, provider_id: &str, signal: &CancellationToken) -> anyhow::Result<()> {
        // Invalidate any full availability pass that started before this credential change.
        let (provider_seq, error_seq) = {
            let mut state = self.state.lock();
            state.availability_refresh_seq += 1;
            let provider_seq = state.provider_availability_seq.get(provider_id).copied().unwrap_or(0) + 1;
            state.provider_availability_seq.insert(provider_id.to_string(), provider_seq);
            state.availability_error_seq += 1;
            (provider_seq, state.availability_error_seq)
        };
        let result: anyhow::Result<()> = async {
            let (available, auth, credential) = futures::future::try_join3(
                self.models.get_available(Some(provider_id), Some(signal.clone())),
                self.models.check_auth(provider_id, Some(signal.clone())),
                self.credentials.read(provider_id, Some(signal)),
            )
            .await?;
            throw_if_aborted(signal)?;
            let all = self.models.get_models(None);
            let mut state = self.state.lock();
            if state.provider_availability_seq.get(provider_id) != Some(&provider_seq) {
                return Ok(());
            }
            let mut configured_providers = state.snapshot.configured_providers.clone();
            let mut stored_providers = state.snapshot.stored_providers.clone();
            let mut auth_by_provider = state.snapshot.auth.clone();
            match auth {
                Some(auth) => {
                    configured_providers.insert(provider_id.to_string());
                    auth_by_provider.insert(provider_id.to_string(), auth);
                }
                None => {
                    configured_providers.shift_remove(provider_id);
                    auth_by_provider.shift_remove(provider_id);
                }
            }
            if credential.is_some() {
                stored_providers.insert(provider_id.to_string());
            } else {
                stored_providers.shift_remove(provider_id);
            }
            let mut available_by_id: HashMap<(String, String), Model> = HashMap::new();
            for model in state
                .snapshot
                .available
                .iter()
                .filter(|model| model.provider != provider_id)
                .cloned()
                .chain(available)
            {
                available_by_id.insert((model.provider.clone(), model.id.clone()), model);
            }
            let available = all
                .iter()
                .filter_map(|model| available_by_id.get(&(model.provider.clone(), model.id.clone())).cloned())
                .collect();
            state.snapshot = ModelRuntimeSnapshot {
                all,
                available,
                configured_providers,
                stored_providers,
                auth: auth_by_provider,
            };
            if error_seq == state.availability_error_seq {
                state.availability_error = None;
            }
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            let mut state = self.state.lock();
            if state.provider_availability_seq.get(provider_id) == Some(&provider_seq)
                && error_seq == state.availability_error_seq
                && !signal.is_cancelled()
            {
                state.availability_error = Some(error_message(error));
            }
        }
        result
    }

    /// The underlying pi-ai `Models` collection.
    pub fn models(&self) -> &Models {
        &self.models
    }

    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.models.get_providers()
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        self.models.get_provider(provider_id)
    }

    pub fn get_models(&self, provider_id: Option<&str>) -> Vec<Model> {
        self.models.get_models(provider_id)
    }

    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.models.get_model(provider_id, model_id)
    }

    pub fn get_models_of_type(&self, r#type: ModelType, provider_id: Option<&str>) -> Vec<AnyModel> {
        self.models.get_models_of_type(r#type, provider_id)
    }

    pub fn get_model_of_type(&self, r#type: ModelType, provider_id: &str, model_id: &str) -> Option<AnyModel> {
        self.models.get_model_of_type(r#type, provider_id, model_id)
    }

    pub fn get_all_models(&self, provider_id: Option<&str>) -> Vec<AnyModel> {
        self.models.get_all_models(provider_id)
    }

    pub async fn get_available_of_type(
        &self,
        r#type: ModelType,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<Vec<AnyModel>> {
        self.models
            .get_available_of_type(r#type, provider_id, options.and_then(|o| o.signal.clone()))
            .await
    }

    pub async fn get_all_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<Vec<AnyModel>> {
        self.models
            .get_all_available(provider_id, options.and_then(|o| o.signal.clone()))
            .await
    }

    pub async fn check_auth(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<Option<AuthCheck>> {
        self.models
            .check_auth(provider_id, options.and_then(|o| o.signal.clone()))
            .await
    }

    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<Vec<Model>> {
        let signal = options.and_then(|o| o.signal.clone());
        if let Some(provider_id) = provider_id {
            let error_seq = {
                let mut state = self.state.lock();
                state.availability_error_seq += 1;
                state.availability_error_seq
            };
            let result = self.models.get_available(Some(provider_id), signal.clone()).await;
            let mut state = self.state.lock();
            match &result {
                Ok(_) if error_seq == state.availability_error_seq => state.availability_error = None,
                Err(error)
                    if error_seq == state.availability_error_seq
                        && !signal.as_ref().is_some_and(CancellationToken::is_cancelled) =>
                {
                    state.availability_error = Some(error_message(error));
                }
                _ => {}
            }
            return result;
        }
        self.queue_availability_refresh(signal).await?;
        Ok(self.state.lock().snapshot.available.clone())
    }

    pub fn get_available_snapshot(&self) -> Vec<Model> {
        self.state.lock().snapshot.available.clone()
    }

    pub fn get_error(&self) -> Option<String> {
        let state = self.state.lock();
        let mut errors = Vec::new();
        if let Some(config_error) = state.config.get_error() {
            errors.push(config_error);
        }
        for (provider_id, error) in &state.composition_errors {
            errors.push(format!("Provider \"{provider_id}\": {error}"));
        }
        if let Some(error) = &state.availability_error {
            errors.push(format!("Availability refresh: {error}"));
        }
        (!errors.is_empty()).then(|| errors.join("\n\n"))
    }

    pub fn get_registered_provider_config(&self, provider_id: &str) -> Option<ProviderConfigInput> {
        self.state.lock().extension_providers.get(provider_id).cloned()
    }

    /// The registered config plus its registration revision, which changes with every
    /// `register_provider` call (the identity of the stored config object in TS).
    pub fn get_registered_provider_config_revision(&self, provider_id: &str) -> Option<(u64, ProviderConfigInput)> {
        let state = self.state.lock();
        let config = state.extension_providers.get(provider_id)?.clone();
        Some((state.extension_provider_revisions.get(provider_id).copied().unwrap_or_default(), config))
    }

    pub fn get_registered_provider_ids(&self) -> Vec<String> {
        let state = self.state.lock();
        let mut ids: IndexSet<String> = state.extension_providers.keys().cloned().collect();
        ids.extend(state.native_extension_providers.keys().cloned());
        ids.into_iter().collect()
    }

    pub fn get_registered_native_provider(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        self.state.lock().native_extension_providers.get(provider_id).cloned()
    }

    /// @internal Compatibility fallback for ModelRegistry when provider auth is unconfigured.
    pub fn get_compatibility_request_config(&self, model: &Model) -> anyhow::Result<CompatibilityRequestConfig> {
        let (config, extension) = {
            let state = self.state.lock();
            (state.config.clone(), state.extension_providers.get(&model.provider).cloned())
        };
        resolve_compatibility_request_config(
            &AnyModel::Chat(model.clone()),
            config.get_provider(&model.provider),
            extension.as_ref(),
        )
    }

    pub fn is_using_o_auth(&self, provider_id: &str) -> bool {
        self.state
            .lock()
            .snapshot
            .auth
            .get(provider_id)
            .is_some_and(|check| check.auth_type == AuthType::OAuth)
    }

    pub fn is_using_subscription(&self, provider_id: &str) -> bool {
        self.is_using_o_auth(provider_id)
            && self
                .models
                .get_provider(provider_id)
                .and_then(|provider| provider.auth().oauth.clone())
                .is_some_and(|oauth| oauth.is_subscription() == Some(true))
    }

    pub fn has_configured_auth(&self, provider_id: &str) -> bool {
        self.state.lock().snapshot.configured_providers.contains(provider_id)
    }

    pub async fn get_auth(
        &self,
        provider_or_model: &ModelRuntimeGetAuthProviderOrModel,
        overrides: Option<&ModelRuntimeAuthOverrides>,
    ) -> anyhow::Result<Option<AuthResult>> {
        let overrides = overrides.cloned().unwrap_or_default();
        let resolution_overrides = AuthResolutionOverrides {
            api_key: overrides.api_key.clone(),
            env: overrides.env.as_ref().map(to_provider_env),
            min_oauth_validity_ms: overrides.min_o_auth_validity_ms,
            signal: overrides.signal.clone(),
        };
        let model = match provider_or_model {
            ModelRuntimeGetAuthProviderOrModel::String(provider_id) => {
                return self.models.get_auth(provider_id, resolution_overrides).await;
            }
            ModelRuntimeGetAuthProviderOrModel::AnyModel(model) => model,
        };
        let Some(mut resolution) = self.models.get_auth_for_model(model, resolution_overrides).await? else {
            return Ok(None);
        };
        let (config, extension) = {
            let state = self.state.lock();
            (state.config.clone(), state.extension_providers.get(model.provider()).cloned())
        };
        let mut env: IndexMap<String, String> = resolution
            .env
            .as_ref()
            .map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        if let Some(extra) = &overrides.env {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let configured = resolve_configured_model_headers(
            model,
            config.get_provider(model.provider()),
            extension.as_ref(),
            Some(&env),
        )?;
        let configured: Option<ProviderHeaders> =
            configured.map(|headers| headers.into_iter().map(|(k, v)| (k, Some(v))).collect());
        resolution.auth.headers = merge_headers(resolution.auth.headers.as_ref(), configured.as_ref());
        Ok(Some(resolution))
    }

    /// Per-provider serialization of credential changes. An operation waits for earlier ones; a
    /// caller aborted before its operation starts stops waiting.
    async fn enqueue_credential_operation<T>(
        &self,
        provider_id: &str,
        signal: &CancellationToken,
        task: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let lock = self
            .credential_operations
            .lock()
            .entry(provider_id.to_string())
            .or_default()
            .clone();
        let guard = tokio::select! {
            guard = lock.lock_owned() => guard,
            _ = signal.cancelled() => return Err(SignalAbortError.into()),
        };
        throw_if_aborted(signal)?;
        let result = task.await;
        drop(guard);
        result
    }

    async fn synchronize_credential_state(
        &self,
        provider_id: &str,
        operation: CredentialSynchronizationOperation,
        credential: Option<Credential>,
        signal: &CancellationToken,
    ) -> anyhow::Result<()> {
        let result: anyhow::Result<()> = async {
            throw_if_aborted(signal)?;
            self.recompose_provider(provider_id);
            let composition_error = self.state.lock().composition_errors.get(provider_id).cloned();
            if let Some(error) = composition_error {
                anyhow::bail!(error);
            }
            let result = self
                .models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(false),
                    providers: Some(vec![provider_id.to_string()]),
                    force: None,
                    signal: Some(signal.clone()),
                })
                .await;
            if result.aborted {
                throw_if_aborted(signal)?;
            }
            if let Some(error) = result.errors.get(provider_id) {
                anyhow::bail!(error.clone());
            }
            self.update_model_snapshot();
            self.refresh_provider_availability(provider_id, signal).await
        }
        .await;
        result.map_err(|cause| {
            CredentialSynchronizationError::new(provider_id, operation, credential, cause).into()
        })
    }

    pub async fn set_runtime_api_key(
        &self,
        provider_id: &str,
        api_key: &str,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<()> {
        let signal = options.and_then(|o| o.signal.clone()).unwrap_or_default();
        self.enqueue_credential_operation(provider_id, &signal, async {
            self.credentials.set_runtime_api_key(provider_id, api_key);
            self.synchronize_credential_state(
                provider_id,
                CredentialSynchronizationOperation::SetRuntimeApiKey,
                Some(Credential::ApiKey(ApiKeyCredential { key: Some(api_key.to_string()), env: None })),
                &signal,
            )
            .await
        })
        .await
    }

    pub async fn remove_runtime_api_key(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<()> {
        let signal = options.and_then(|o| o.signal.clone()).unwrap_or_default();
        self.enqueue_credential_operation(provider_id, &signal, async {
            self.credentials.remove_runtime_api_key(provider_id);
            self.synchronize_credential_state(
                provider_id,
                CredentialSynchronizationOperation::RemoveRuntimeApiKey,
                None,
                &signal,
            )
            .await
        })
        .await
    }

    pub async fn list_credentials(&self, options: Option<&AuthOperationOptions>) -> anyhow::Result<Vec<CredentialInfo>> {
        let signal = options.and_then(|o| o.signal.clone());
        self.credentials.list(signal.as_ref()).await
    }

    pub fn get_provider_auth_status(&self, provider_id: &str) -> AuthStatus {
        if self.credentials.has_runtime_api_key(provider_id) {
            return AuthStatus::configured(AuthStatusSource::Runtime, None);
        }
        let (stored, config, extension, check) = {
            let state = self.state.lock();
            (
                state.snapshot.stored_providers.contains(provider_id),
                state.config.clone(),
                state.extension_providers.get(provider_id).cloned(),
                state.snapshot.auth.get(provider_id).cloned(),
            )
        };
        if stored {
            return AuthStatus::configured(AuthStatusSource::Stored, None);
        }
        if let Some(configured) = configured_request_auth_status(config.get_provider(provider_id), extension.as_ref()) {
            return configured;
        }
        match check {
            Some(check) => AuthStatus::configured(AuthStatusSource::Environment, check.source),
            None => AuthStatus::unconfigured(),
        }
    }

    async fn prepare_request(&self, model: AnyModel, request: ModelsRequestOptions) -> anyhow::Result<PreparedRequest> {
        let provider = self.models.get_provider(model.provider()).ok_or_else(|| {
            ModelsError::new(ModelsErrorCode::Provider, format!("Unknown provider: {}", model.provider()))
        })?;
        let options = request.options;
        let resolution = self
            .get_auth(
                &ModelRuntimeGetAuthProviderOrModel::AnyModel(model.clone()),
                Some(&ModelRuntimeAuthOverrides {
                    api_key: options.api_key.clone(),
                    env: options
                        .env
                        .as_ref()
                        .map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                    signal: options.signal.clone(),
                    min_o_auth_validity_ms: None,
                }),
            )
            .await?
            .ok_or_else(|| {
                ModelsError::new(ModelsErrorCode::Auth, format!("Provider is not configured: {}", model.provider()))
            })?;
        let mut request_model = model;
        if let Some(base_url) = resolution.auth.base_url.clone().filter(|url| !url.is_empty()) {
            request_model.set_base_url(base_url);
        }
        let session_headers: Option<ProviderHeaders> = get_open_code_session_headers(
            ClientIdentityTarget { provider: request_model.provider(), base_url: request_model.base_url() },
            options.session_id.as_deref(),
        )
        .map(|headers| headers.into_iter().map(|(k, v)| (k, Some(v))).collect());
        let mut headers = merge_headers(
            merge_headers(session_headers.as_ref(), resolution.auth.headers.as_ref()).as_ref(),
            options.headers.as_ref(),
        );
        if let Some(transform) = &request.transform_headers {
            headers = Some(transform(headers.unwrap_or_default()).await);
        }
        let env = if resolution.env.is_some() || options.env.is_some() {
            let mut env = resolution.env.clone().unwrap_or_default();
            env.extend(options.env.clone().unwrap_or_default());
            Some(env)
        } else {
            None
        };
        let mut options = options;
        options.api_key = options.api_key.clone().or(resolution.auth.api_key.clone());
        options.headers = headers;
        options.env = env;
        Ok(PreparedRequest { provider, model: request_model, options })
    }

    fn this(&self) -> anyhow::Result<Arc<ModelRuntime>> {
        self.weak
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Model runtime is no longer available"))
    }

    pub fn stream(&self, model: Model, context: Context, options: Option<ModelsRequestOptions>) -> AssistantMessageEventStream {
        let transcript = normalize_context(&context);
        let this = self.this();
        let request = options.unwrap_or_default();
        let owned = model.clone();
        lazy_stream(
            &model,
            async move {
                let this = this?;
                assert_chat_model(&owned)?;
                let prepared = this.prepare_request(AnyModel::Chat(owned), request).await?;
                let model = prepared.model.into_chat().expect("chat model stays chat");
                Ok(prepared.provider.stream(&model, &transcript, prepared.options))
            }
            .boxed(),
            None,
        )
    }

    pub async fn complete(&self, model: Model, context: Context, options: Option<ModelsRequestOptions>) -> AssistantMessage {
        self.stream(model, context, options).result().await
    }

    pub fn stream_simple(
        &self,
        model: Model,
        context: Context,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantMessageEventStream {
        let transcript = normalize_context(&context);
        let this = self.this();
        let request = options.unwrap_or_default();
        let owned = model.clone();
        if is_virtual_model(&model) {
            // Requests outside the agent loop are routed here. Callers sized them before routing, so
            // cap the output budget to the routed model.
            return lazy_stream(
                &model,
                async move {
                    let this = this?;
                    let options = request.options.clone();
                    let route = this
                        .resolve_model(
                            owned.clone(),
                            &transcript.messages,
                            &ModelRuntimeResolveModelOptions {
                                reason: ModelRouteReason::Direct,
                                thinking_level: options
                                    .reasoning
                                    .map(ModelThinkingLevel::from)
                                    .unwrap_or(ModelThinkingLevel::Off),
                                signal: options.signal.clone(),
                                failed: None,
                                state: None,
                            },
                        )
                        .await?;
                    let limit = route.model.max_tokens;
                    let max_tokens = match options.max_tokens {
                        Some(requested) if requested > 0 && limit > 0 => Some(requested.min(limit)),
                        other => other,
                    };
                    let reasoning = route.thinking_level.thinking();
                    let mut next = options;
                    // Caller credentials were resolved for the virtual model's provider. Another provider
                    // resolves its own, so they are not sent to the wrong vendor.
                    if route.model.provider != owned.provider {
                        next.api_key = None;
                        next.headers = None;
                        next.env = None;
                    }
                    next.max_tokens = max_tokens;
                    next.reasoning = reasoning;
                    Ok(this.stream_simple(
                        route.model,
                        context,
                        Some(ModelsRequestOptions { options: next, transform_headers: request.transform_headers }),
                    ))
                }
                .boxed(),
                None,
            );
        }
        lazy_stream(
            &model,
            async move {
                let this = this?;
                assert_chat_model(&owned)?;
                let prepared = this.prepare_request(AnyModel::Chat(owned), request).await?;
                let model = prepared.model.into_chat().expect("chat model stays chat");
                Ok(prepared.provider.stream_simple(&model, &transcript, prepared.options))
            }
            .boxed(),
            None,
        )
    }

    pub async fn complete_simple(
        &self,
        model: Model,
        context: Context,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options).result().await
    }

    pub fn stream_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantMessageEventStream {
        let this = self.this();
        let request = options.unwrap_or_default();
        let owned = model.clone();
        lazy_stream(
            &model,
            async move {
                let this = this?;
                assert_chat_model(&owned)?;
                let prepared = this.prepare_request(AnyModel::Chat(owned.clone()), request).await?;
                if !prepared.provider.supports_fetch_deferred() {
                    return Err(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!("Provider {} does not support deferred responses", owned.provider),
                    )
                    .into());
                }
                let model = prepared.model.into_chat().expect("chat model stays chat");
                Ok(prepared.provider.fetch_deferred(&model, &handle, prepared.options))
            }
            .boxed(),
            None,
        )
    }

    pub async fn fetch_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantMessage {
        self.stream_deferred(model, handle, options).result().await
    }

    pub async fn cancel_deferred(
        &self,
        model: Model,
        handle: DeferredHandle,
        options: Option<ModelsRequestOptions>,
    ) -> anyhow::Result<()> {
        assert_chat_model(&model)?;
        let prepared = self
            .prepare_request(AnyModel::Chat(model.clone()), options.unwrap_or_default())
            .await?;
        if !prepared.provider.supports_cancel_deferred() {
            return Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} does not support deferred responses", model.provider),
            )
            .into());
        }
        let model = prepared.model.into_chat().expect("chat model stays chat");
        prepared.provider.cancel_deferred(&model, &handle, prepared.options).await
    }

    /// Generate images with request-time authentication. Never fails: errors become results.
    pub async fn generate_images(
        &self,
        model: ImageModel,
        context: ImagesContext,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantImages {
        let request = options.unwrap_or_default();
        let signal = request.options.signal.clone();
        let result: anyhow::Result<AssistantImages> = async {
            assert_image_model(&model)?;
            let prepared = self.prepare_request(AnyModel::Image(model.clone()), request).await?;
            if !prepared.provider.supports_images() {
                return Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} does not support image generation", model.provider),
                )
                .into());
            }
            let AnyModel::Image(request_model) = prepared.model else { unreachable!("image model stays image") };
            Ok(prepared.provider.generate_images(&request_model, &context, prepared.options).await)
        }
        .await;
        match result {
            Ok(images) => images,
            Err(error) => image_error_result(&model, &error, signal.is_some_and(|s| s.is_cancelled())),
        }
    }

    /// Classify with request-time authentication. Never fails: errors become results.
    pub async fn classify(
        &self,
        model: ClassifierModel,
        context: ClassifierContext,
        options: Option<ModelsRequestOptions>,
    ) -> ClassifierResult {
        let request = options.unwrap_or_default();
        let signal = request.options.signal.clone();
        let result: anyhow::Result<ClassifierResult> = async {
            assert_classifier_model(&model)?;
            assert_classifier_input_supported(&model, &context)?;
            let prepared = self.prepare_request(AnyModel::Classifier(model.clone()), request).await?;
            if !prepared.provider.supports_classify() {
                return Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} does not support classification", model.provider),
                )
                .into());
            }
            let AnyModel::Classifier(request_model) = prepared.model else {
                unreachable!("classifier model stays classifier")
            };
            Ok(prepared.provider.classify(&request_model, &context, prepared.options).await)
        }
        .await;
        match result {
            Ok(result) => result,
            Err(error) => classifier_error_result(&model, &error, signal.is_some_and(|s| s.is_cancelled())),
        }
    }

    /// Run a provider login flow, persist the credential, and synchronize the snapshot.
    /// `signal` is the interaction's cancellation signal.
    pub async fn login(
        &self,
        provider_id: &str,
        r#type: AuthType,
        interaction: Arc<dyn AuthInteraction>,
        signal: Option<CancellationToken>,
        options: Option<LoginOptions>,
    ) -> anyhow::Result<Credential> {
        let signal = signal.unwrap_or_default();
        self.enqueue_credential_operation(provider_id, &signal, async {
            let credential = self
                .models
                .login(provider_id, r#type, interaction, Some(signal.clone()), options)
                .await?;
            self.synchronize_credential_state(
                provider_id,
                CredentialSynchronizationOperation::Login,
                Some(credential.clone()),
                &signal,
            )
            .await?;
            Ok(credential)
        })
        .await
    }

    pub async fn logout(&self, provider_id: &str, options: Option<&AuthOperationOptions>) -> anyhow::Result<()> {
        let signal = options.and_then(|o| o.signal.clone()).unwrap_or_default();
        self.enqueue_credential_operation(provider_id, &signal, async {
            self.models.logout(provider_id, Some(signal.clone())).await?;
            self.synchronize_credential_state(provider_id, CredentialSynchronizationOperation::Logout, None, &signal)
                .await
        })
        .await
    }

    pub async fn refresh(&self, options: Option<ModelsRefreshOptions>) -> anyhow::Result<ModelsRefreshResult> {
        let options = options.unwrap_or_default();
        let lock = self.refresh_lock.clone();
        let guard = match options.signal.clone() {
            Some(signal) => tokio::select! {
                guard = lock.lock_owned() => guard,
                _ = signal.cancelled() => return Ok(ModelsRefreshResult { aborted: true, errors: Default::default() }),
            },
            None => lock.lock_owned().await,
        };
        self.refresh_locked(options, guard).await
    }

    /// `refresh()` body; runs while holding the refresh lock.
    async fn refresh_locked(
        &self,
        options: ModelsRefreshOptions,
        _refresh_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> anyhow::Result<ModelsRefreshResult> {
        let config = ModelConfig::load(self.models_path.as_deref()).await;
        self.state.lock().config = config;
        self.configure_radius_providers();
        let selected: Option<Vec<String>> = options.providers.as_ref().map(|providers| {
            providers.iter().cloned().collect::<IndexSet<String>>().into_iter().collect()
        });
        match &selected {
            Some(providers) => {
                for provider_id in providers {
                    self.recompose_provider(provider_id);
                }
                self.update_model_snapshot();
            }
            None => self.rebuild_providers(),
        }
        let refresh_options = ModelsRefreshOptions {
            allow_network: Some(options.allow_network.unwrap_or(self.model_network_enabled)),
            ..options.clone()
        };
        let result = self.models.refresh(refresh_options).await;
        let mut errors = result.errors.clone();
        self.update_model_snapshot();
        let caller_aborted = || options.signal.as_ref().is_some_and(CancellationToken::is_cancelled);
        match &selected {
            Some(providers) => {
                let signal = options.signal.clone().unwrap_or_default();
                let results = futures::future::join_all(
                    providers.iter().map(|provider_id| {
                        let signal = signal.clone();
                        async move { (provider_id.clone(), self.refresh_provider_availability(provider_id, &signal).await) }
                    }),
                )
                .await;
                for (provider_id, result) in results {
                    if let Err(error) = result
                        && !caller_aborted()
                    {
                        errors.insert(provider_id, error_message(&error));
                    }
                }
            }
            None => {
                // Availability errors are recorded by the latest pass; refreshed models remain usable.
                let _ = self.queue_availability_refresh(options.signal.clone()).await;
            }
        }
        Ok(ModelsRefreshResult { aborted: result.aborted || caller_aborted(), errors })
    }

    fn spawn_offline_refresh(&self) {
        use std::sync::atomic::Ordering;
        let Some(this) = self.weak.upgrade() else { return };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self.background_refresh_pending.swap(true, Ordering::SeqCst) {
            // A queued pass has not started yet and will observe this change.
            return;
        }
        let held = self.refresh_lock.clone().try_lock_owned().ok();
        spawn_detached(
            async move {
                let guard = match held {
                    Some(guard) => guard,
                    None => this.refresh_lock.clone().lock_owned().await,
                };
                this.background_refresh_pending.store(false, Ordering::SeqCst);
                let _ = this
                    .refresh_locked(ModelsRefreshOptions { allow_network: Some(false), ..Default::default() }, guard)
                    .await;
            }
            .boxed(),
        );
    }

    pub fn register_native_provider(&self, provider: Arc<dyn Provider>) -> anyhow::Result<()> {
        if provider.id().trim().is_empty() {
            anyhow::bail!("Provider id must not be empty.");
        }
        let provider_id = provider.id().to_string();
        let auth_type = if provider.auth().oauth.is_some() && provider.auth().api_key.is_none() {
            AuthType::OAuth
        } else {
            AuthType::ApiKey
        };
        {
            let _composition = self.composition.lock();
            {
                let mut state = self.state.lock();
                state.extension_providers.shift_remove(&provider_id);
                state.extension_provider_revisions.remove(&provider_id);
                state.native_extension_providers.insert(provider_id.clone(), provider);
            }
            self.recompose_provider(&provider_id);
        }
        self.update_model_snapshot();
        let config = self.state.lock().config.clone();
        self.mark_provisionally_configured(
            &provider_id,
            configured_request_auth_status(config.get_provider(&provider_id), None),
            auth_type,
        );
        self.spawn_offline_refresh();
        Ok(())
    }

    /// Mark a newly registered provider as configured when it has a stored credential or a configured
    /// API key. Availability checks run asynchronously, and callers such as initial model selection
    /// read the snapshot before they finish. The next availability pass replaces this entry.
    fn mark_provisionally_configured(&self, provider_id: &str, configured_status: Option<AuthStatus>, auth_type: AuthType) {
        let mut state = self.state.lock();
        if !state.snapshot.stored_providers.contains(provider_id)
            && !configured_status.is_some_and(|status| status.configured)
        {
            return;
        }
        state.snapshot.configured_providers.insert(provider_id.to_string());
        // Never clobber a real check result.
        if !state.snapshot.auth.contains_key(provider_id) {
            state.snapshot.auth.insert(
                provider_id.to_string(),
                AuthCheck { source: Some("configured provider".to_string()), auth_type },
            );
        }
        let configured = state.snapshot.configured_providers.clone();
        state.snapshot.available = state
            .snapshot
            .all
            .iter()
            .filter(|model| configured.contains(&model.provider))
            .cloned()
            .collect();
    }

    pub fn register_provider(&self, provider_id: &str, config: &ProviderConfigInput) -> anyhow::Result<()> {
        // Validate the incoming registration on its own, like the legacy registry:
        // a broken re-registration must throw without touching the stored config.
        let (base, models_config) = {
            let state = self.state.lock();
            (state.builtins.get(provider_id).cloned(), state.config.clone())
        };
        validate_extension_provider(provider_id, base, models_config.get_provider(provider_id), config)?;
        // Re-registration merges defined values over the previous registration and
        // preserves undefined ones, matching the legacy ModelRegistry contract.
        let composition = self.composition.lock();
        let effective = {
            let mut state = self.state.lock();
            state.native_extension_providers.shift_remove(provider_id);
            let effective = match state.extension_providers.get(provider_id) {
                Some(previous) => merge_provider_config(previous, config),
                None => config.clone(),
            };
            state.extension_providers.insert(provider_id.to_string(), effective.clone());
            state
                .extension_provider_revisions
                .insert(provider_id.to_string(), PROVIDER_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            effective
        };
        self.recompose_provider(provider_id);
        drop(composition);
        self.update_model_snapshot();
        let auth_type = if effective.oauth.is_some() && effective.api_key.is_none() {
            AuthType::OAuth
        } else {
            AuthType::ApiKey
        };
        self.mark_provisionally_configured(
            provider_id,
            configured_request_auth_status(models_config.get_provider(provider_id), Some(&effective)),
            auth_type,
        );
        self.spawn_offline_refresh();
        Ok(())
    }

    pub fn unregister_provider(&self, provider_id: &str) {
        {
            let _composition = self.composition.lock();
            {
                let mut state = self.state.lock();
                state.extension_providers.shift_remove(provider_id);
                state.extension_provider_revisions.remove(provider_id);
                state.native_extension_providers.shift_remove(provider_id);
            }
            self.recompose_provider(provider_id);
        }
        self.update_model_snapshot();
        self.spawn_offline_refresh();
    }

    /// Register a virtual model under `definition.provider`, which may also list physical models or
    /// several virtual models. Re-registering the same provider and id replaces the virtual model.
    /// Throws when the id belongs to a physical model of that provider.
    pub fn register_virtual_model(&self, definition: &VirtualModelDefinition) -> anyhow::Result<()> {
        let provider_id = definition.provider.clone();
        let id = definition.id.clone();
        if provider_id.trim().is_empty() || id.trim().is_empty() {
            anyhow::bail!("Virtual model provider and id must not be empty.");
        }
        if let Some(existing) = self.models.get_model(&provider_id, &id)
            && !is_virtual_model(&existing)
        {
            anyhow::bail!("Virtual model {provider_id}/{id} conflicts with a physical model.");
        }
        let composition = self.composition.lock();
        self.state
            .lock()
            .virtual_models
            .entry(provider_id.clone())
            .or_default()
            .insert(
                id,
                RegisteredVirtualModel {
                    model: create_virtual_model(&definition.without_route()),
                    route: definition.route.clone(),
                },
            );
        let recomposed = self.recompose_provider(&provider_id);
        drop(composition);
        if recomposed.is_none() {
            let mut state = self.state.lock();
            if !state.snapshot.configured_providers.contains(&provider_id) {
                // A provider of only virtual models needs no credentials. Mark it configured now: session
                // restore checks auth before the refresh below lands.
                state.snapshot.auth.insert(
                    provider_id.clone(),
                    AuthCheck { source: Some("virtual".to_string()), auth_type: AuthType::ApiKey },
                );
                state.snapshot.configured_providers.insert(provider_id.clone());
            }
        }
        self.update_model_snapshot();
        self.spawn_offline_refresh();
        Ok(())
    }

    pub fn unregister_virtual_model(&self, provider_id: &str, id: &str) {
        let composition = self.composition.lock();
        {
            let mut state = self.state.lock();
            let Some(models) = state.virtual_models.get_mut(provider_id) else { return };
            if models.shift_remove(id).is_none() {
                return;
            }
            if models.is_empty() {
                state.virtual_models.shift_remove(provider_id);
            }
        }
        self.recompose_provider(provider_id);
        drop(composition);
        self.update_model_snapshot();
        self.spawn_offline_refresh();
    }

    /// Ask a virtual model's router for the model and thinking level of one request. The router must
    /// return a physical catalog model whose provider has credentials; the thinking level is clamped
    /// to that model. Throws when routing fails.
    ///
    /// `previous` reports the latest successful response in `messages`. A retry passes the failed
    /// response as `options.failed`; `messages` no longer contains it. `options.state` is the router
    /// state stored by the caller, which also stores the returned state.
    pub async fn resolve_model(
        &self,
        model: Model,
        messages: &[Message],
        options: &ModelRuntimeResolveModelOptions,
    ) -> anyhow::Result<ModelRoute> {
        let name = format!("Virtual model {}/{}", model.provider, model.id);
        let route_fn = self
            .state
            .lock()
            .virtual_models
            .get(&model.provider)
            .and_then(|models| models.get(&model.id))
            .map(|entry| entry.route.clone());
        let Some(route_fn) = route_fn else {
            anyhow::bail!("{name} is not registered.");
        };
        let latest = find_latest_llm_response(messages);
        let previous = latest.as_ref().and_then(|latest| {
            self.get_physical_model(&latest.provider, &latest.model)
                .map(|model| ModelRouteRequestPrevious { model, thinking_level: latest.thinking_level })
        });
        // A failed routing attempt names the virtual model; there is no physical request to report.
        let failed = options.failed.as_ref().and_then(|failed| {
            self.get_physical_model(&failed.provider, &failed.model).map(|model| ModelRouteRequestFailed {
                model,
                thinking_level: failed.thinking_level,
                message: failed.clone(),
            })
        });
        let route = route_fn(ModelRouteRequest {
            model: model.clone(),
            thinking_level: options.thinking_level,
            reason: options.reason,
            previous,
            failed,
            state: options.state.clone(),
            messages: messages.to_vec(),
            signal: options.signal.clone(),
        })
        .await?;
        let routed = format!("{name} routed to {}/{}", route.model.provider, route.model.id);
        let Some(target) = self.get_physical_model(&route.model.provider, &route.model.id) else {
            anyhow::bail!("{routed}, which is not a physical model.");
        };
        if !self.has_configured_auth(&target.provider) {
            anyhow::bail!("{routed}, which has no credentials.");
        }
        let thinking_level = clamp_thinking_level(&target, route.thinking_level);
        Ok(ModelRoute { model: target, thinking_level, state: route.state })
    }

    /// A catalog chat model that is not virtual.
    pub fn get_physical_model(&self, provider_id: &str, model_id: &str) -> Option<Model> {
        self.models
            .get_model(provider_id, model_id)
            .filter(|model| !is_virtual_model(model))
    }
}

/// `{ ...previous, ...defined(config) }`: defined values of a re-registration replace the
/// previous ones; undefined values keep them.
fn merge_provider_config(previous: &ProviderConfigInput, config: &ProviderConfigInput) -> ProviderConfigInput {
    ProviderConfigInput {
        name: config.name.clone().or_else(|| previous.name.clone()),
        base_url: config.base_url.clone().or_else(|| previous.base_url.clone()),
        api_key: config.api_key.clone().or_else(|| previous.api_key.clone()),
        api: config.api.clone().or_else(|| previous.api.clone()),
        stream_simple: config.stream_simple.clone().or_else(|| previous.stream_simple.clone()),
        images: config.images.clone().or_else(|| previous.images.clone()),
        classifiers: config.classifiers.clone().or_else(|| previous.classifiers.clone()),
        headers: config.headers.clone().or_else(|| previous.headers.clone()),
        auth_header: config.auth_header.or(previous.auth_header),
        models: config.models.clone().or_else(|| previous.models.clone()),
        refresh_models: config.refresh_models.clone().or_else(|| previous.refresh_models.clone()),
        oauth: config.oauth.clone().or_else(|| previous.oauth.clone()),
    }
}
