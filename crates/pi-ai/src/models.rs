//! Providers and the `Models` collection (port of `models.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::api::lazy::{error_stream, lazy_stream};
use crate::auth::context::default_provider_auth_context;
use crate::auth::credential_store::InMemoryCredentialStore;
use crate::auth::resolve::{
    AuthResolutionOverrides, race_with_signal, refresh_stored_oauth_credential, resolve_provider_auth,
};
use crate::auth::types::{
    ApiKeyCredential, ApiKeyResolveInput, AuthCheck, AuthContext, AuthInteraction, AuthResult, AuthType, Credential,
    CredentialStore, LoginOptions, ProviderAuth, ProviderAuthInteraction,
};
use crate::telemetry::observe_model_request;
use crate::models_store::{InMemoryModelsStore, ModelsStore, ModelsStoreEntry};
use crate::types::{
    AnyModel, AssistantImages, AssistantMessage, AssistantMessageEventStream, ClassifierContext, ClassifierModel,
    ClassifierResult, Context, DeferredHandle, ImageModel, ImagesContext, Model, ModelCost, ModelThinkingLevel,
    ModelType, ProviderHeaders, StreamOptions, TranscriptContext, Usage, UsageCost,
};
use crate::utils::identity::{ClientIdentity, ClientIdentityTarget, get_open_code_session_headers, merge_client_identity};
use crate::utils::model_operations::{
    assert_chat_model, assert_classifier_input_supported, assert_classifier_model, assert_image_model,
    classifier_error_result, image_error_result,
};
pub use crate::utils::models_error::{ModelsError, ModelsErrorCode};
use crate::utils::abort::SignalAbortError;
use crate::utils::transcript::normalize_context;

// ---------------------------------------------------------------------------
// API implementation contracts
// ---------------------------------------------------------------------------

/// The uniform stream contract of an API implementation (`ProviderStreams`).
///
/// Implementations must return a stream immediately; request failures are encoded in
/// the stream as an `error` event.
pub trait ProviderStreams: Send + Sync {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: StreamOptions,
    ) -> AssistantMessageEventStream;

    fn supports_fetch_deferred(&self) -> bool {
        false
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        _handle: &DeferredHandle,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        error_stream(model, anyhow::anyhow!("API does not support deferred responses"))
    }

    fn supports_cancel_deferred(&self) -> bool {
        false
    }

    fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
        _options: StreamOptions,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Err(anyhow::anyhow!("API cannot cancel deferred responses")) })
    }
}

/// Image-generation API implementation.
#[async_trait]
pub trait ProviderImages: Send + Sync {
    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages;
}

/// Classifier API implementation.
#[async_trait]
pub trait ProviderClassifier: Send + Sync {
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult;
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Generation-checked publication of refreshed models.
#[derive(Default)]
pub struct ModelsPublication {
    /// `None`: leave storage unchanged; `Some(None)`: delete; `Some(Some(entry))`: persist.
    pub persist: Option<Option<ModelsStoreEntry>>,
    /// Synchronous update of provider-private in-memory catalog state.
    pub update: Option<Box<dyn FnOnce() + Send>>,
}

pub type PublishFn = Arc<dyn Fn(ModelsPublication) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>;

/// Context passed to dynamic providers' `refresh_models`.
#[derive(Clone)]
pub struct RefreshModelsContext {
    /// Effective configured credential.
    pub credential: Option<Credential>,
    /// Provider-scoped catalog snapshot captured before this refresh phase.
    pub stored: Option<ModelsStoreEntry>,
    pub publish: PublishFn,
    /// False during offline/cache-only initialization.
    pub allow_network: bool,
    /// Bypass provider freshness checks.
    pub force: Option<bool>,
    pub signal: CancellationToken,
    /// Base URL of the registered provider.
    pub base_url: Option<String>,
}

/// A provider: id/name/base metadata, auth methods, model listing and operations.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn base_url(&self) -> Option<&str> {
        None
    }
    fn headers(&self) -> Option<&ProviderHeaders> {
        None
    }
    fn auth(&self) -> &ProviderAuth;

    /// Current known chat models.
    fn get_models(&self) -> Vec<Model>;

    /// Current known models of every type. Defaults to the chat models.
    fn get_all_models(&self) -> Vec<AnyModel> {
        self.get_models().into_iter().map(AnyModel::Chat).collect()
    }

    /// Whether the provider has a dynamic model list (`refreshModels`).
    fn supports_refresh(&self) -> bool {
        false
    }

    async fn refresh_models(&self, _context: RefreshModelsContext) -> anyhow::Result<()> {
        Ok(())
    }

    /// Credential-specific chat model availability. `None` = no filter.
    fn filter_models(&self, _models: &[Model], _credential: Option<&Credential>) -> Option<Vec<Model>> {
        None
    }

    /// Credential-specific availability across every model type. `None` = no filter.
    fn filter_all_models(&self, _models: &[AnyModel], _credential: Option<&Credential>) -> Option<Vec<AnyModel>> {
        None
    }

    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream;

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions)
    -> AssistantMessageEventStream;

    fn supports_fetch_deferred(&self) -> bool {
        false
    }

    fn fetch_deferred(&self, model: &Model, _handle: &DeferredHandle, _options: StreamOptions) -> AssistantMessageEventStream {
        error_stream(
            model,
            ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} does not support deferred responses", model.provider),
            )
            .into(),
        )
    }

    fn supports_cancel_deferred(&self) -> bool {
        false
    }

    async fn cancel_deferred(&self, model: &Model, _handle: &DeferredHandle, _options: StreamOptions) -> anyhow::Result<()> {
        Err(ModelsError::new(
            ModelsErrorCode::Provider,
            format!("Provider {} does not support deferred responses", model.provider),
        )
        .into())
    }

    fn supports_images(&self) -> bool {
        false
    }

    async fn generate_images(&self, model: &ImageModel, _context: &ImagesContext, _options: StreamOptions) -> AssistantImages {
        image_error_result(
            model,
            &ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} does not support image generation", model.provider),
            ),
            false,
        )
    }

    fn supports_classify(&self) -> bool {
        false
    }

    async fn classify(&self, model: &ClassifierModel, _context: &ClassifierContext, _options: StreamOptions) -> ClassifierResult {
        classifier_error_result(
            model,
            &ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} does not support classification", model.provider),
            ),
            false,
        )
    }
}

// ---------------------------------------------------------------------------
// createProvider
// ---------------------------------------------------------------------------

/// Chat implementation of a provider: one for all models, or one per `model.api`.
#[derive(Clone)]
pub enum ProviderApi {
    Single(Arc<dyn ProviderStreams>),
    ByApi(IndexMap<String, Arc<dyn ProviderStreams>>),
}

pub type FetchModelsFn =
    Arc<dyn Fn(RefreshModelsContext) -> BoxFuture<'static, anyhow::Result<Vec<AnyModel>>> + Send + Sync>;
pub type FilterModelsFn = Arc<dyn Fn(&[Model], Option<&Credential>) -> Vec<Model> + Send + Sync>;
pub type FilterAllModelsFn = Arc<dyn Fn(&[AnyModel], Option<&Credential>) -> Vec<AnyModel> + Send + Sync>;

/// Input of [`create_provider`].
#[derive(Clone, Default)]
pub struct CreateProviderOptions {
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub auth: ProviderAuth,
    /// Static baseline models of every type.
    pub models: Vec<AnyModel>,
    pub fetch_models: Option<FetchModelsFn>,
    pub filter_models: Option<FilterModelsFn>,
    pub filter_all_models: Option<FilterAllModelsFn>,
    pub api: Option<ProviderApi>,
    pub images: IndexMap<String, Arc<dyn ProviderImages>>,
    pub classifiers: IndexMap<String, Arc<dyn ProviderClassifier>>,
}

struct DefinedProvider {
    id: String,
    name: String,
    base_url: Option<String>,
    headers: Option<ProviderHeaders>,
    auth: ProviderAuth,
    baseline: Vec<AnyModel>,
    dynamic: Arc<RwLock<Vec<AnyModel>>>,
    fetch_models: Option<FetchModelsFn>,
    filter_models_fn: Option<FilterModelsFn>,
    filter_all_models_fn: Option<FilterAllModelsFn>,
    api: Option<ProviderApi>,
    images: IndexMap<String, Arc<dyn ProviderImages>>,
    classifiers: IndexMap<String, Arc<dyn ProviderClassifier>>,
}

impl DefinedProvider {
    fn current_models(&self) -> Vec<AnyModel> {
        let mut merged = self.baseline.clone();
        for model in self.dynamic.read().iter() {
            match merged.iter().position(|e| e.model_type() == model.model_type() && e.id() == model.id()) {
                Some(index) => merged[index] = model.clone(),
                None => merged.push(model.clone()),
            }
        }
        merged
    }

    fn api_for(&self, model: &Model) -> Option<Arc<dyn ProviderStreams>> {
        match &self.api {
            Some(ProviderApi::Single(s)) => Some(s.clone()),
            Some(ProviderApi::ByApi(map)) => map.get(&model.api).cloned(),
            None => None,
        }
    }

    fn all_streams(&self) -> Vec<Arc<dyn ProviderStreams>> {
        match &self.api {
            Some(ProviderApi::Single(s)) => vec![s.clone()],
            Some(ProviderApi::ByApi(map)) => map.values().cloned().collect(),
            None => Vec::new(),
        }
    }

    fn missing_api_stream(&self, model: &Model) -> AssistantMessageEventStream {
        error_stream(
            model,
            ModelsError::new(
                ModelsErrorCode::Stream,
                format!("Provider {} has no API implementation for \"{}\"", self.id, model.api),
            )
            .into(),
        )
    }
}

fn has_known_model_type(model: &AnyModel) -> bool {
    matches!(model.model_type(), ModelType::Chat | ModelType::Image | ModelType::Classifier)
}

#[async_trait]
impl Provider for DefinedProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }
    fn headers(&self) -> Option<&ProviderHeaders> {
        self.headers.as_ref()
    }
    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.current_models().into_iter().filter_map(AnyModel::into_chat).collect()
    }

    fn get_all_models(&self) -> Vec<AnyModel> {
        self.current_models()
    }

    fn supports_refresh(&self) -> bool {
        self.fetch_models.is_some()
    }

    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        let Some(fetch_models) = self.fetch_models.clone() else { return Ok(()) };
        if let Some(stored) = &context.stored {
            let restored: Vec<AnyModel> = stored.models.iter().filter(|m| m.provider() == self.id).cloned().collect();
            let dynamic = self.dynamic.clone();
            let published = (context.publish)(ModelsPublication {
                persist: None,
                update: Some(Box::new(move || *dynamic.write() = restored)),
            })
            .await?;
            if !published {
                return Ok(());
            }
        }
        if !context.allow_network || context.signal.is_cancelled() {
            return Ok(());
        }
        let fetched = fetch_models(context.clone()).await?;
        if context.signal.is_cancelled() {
            return Ok(());
        }
        let refreshed: Vec<AnyModel> = fetched.into_iter().filter(has_known_model_type).collect();
        let dynamic = self.dynamic.clone();
        let persisted = refreshed.clone();
        (context.publish)(ModelsPublication {
            persist: Some(Some(ModelsStoreEntry {
                models: persisted,
                checked_at: Some(crate::utils::now_ms()),
                ..Default::default()
            })),
            update: Some(Box::new(move || *dynamic.write() = refreshed)),
        })
        .await?;
        Ok(())
    }

    fn filter_models(&self, models: &[Model], credential: Option<&Credential>) -> Option<Vec<Model>> {
        self.filter_models_fn.as_ref().map(|f| f(models, credential))
    }

    fn filter_all_models(&self, models: &[AnyModel], credential: Option<&Credential>) -> Option<Vec<AnyModel>> {
        self.filter_all_models_fn.as_ref().map(|f| f(models, credential))
    }

    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(streams) => streams.stream(model, context, options),
            None => self.missing_api_stream(model),
        }
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        match self.api_for(model) {
            Some(streams) => streams.stream_simple(model, context, options),
            None => self.missing_api_stream(model),
        }
    }

    fn supports_fetch_deferred(&self) -> bool {
        self.all_streams().iter().any(|s| s.supports_fetch_deferred())
    }

    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        match self.api_for(model).filter(|s| s.supports_fetch_deferred()) {
            Some(streams) => streams.fetch_deferred(model, handle, options),
            None => error_stream(
                model,
                ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} does not support deferred responses for \"{}\"", self.id, model.api),
                )
                .into(),
            ),
        }
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.all_streams().iter().any(|s| s.supports_cancel_deferred())
    }

    async fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> anyhow::Result<()> {
        match self.api_for(model).filter(|s| s.supports_cancel_deferred()) {
            Some(streams) => streams.cancel_deferred(model, handle, options).await,
            None => Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} cannot cancel deferred responses for \"{}\"", self.id, model.api),
            )
            .into()),
        }
    }

    fn supports_images(&self) -> bool {
        !self.images.is_empty()
    }

    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        match self.images.get(&model.api) {
            Some(implementation) => implementation.generate_images(model, context, options).await,
            None => image_error_result(
                model,
                &ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} has no image generation implementation for \"{}\"", self.id, model.api),
                ),
                false,
            ),
        }
    }

    fn supports_classify(&self) -> bool {
        !self.classifiers.is_empty()
    }

    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        match self.classifiers.get(&model.api) {
            Some(implementation) => implementation.classify(model, context, options).await,
            None => classifier_error_result(
                model,
                &ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} has no classifier implementation for \"{}\"", self.id, model.api),
                ),
                false,
            ),
        }
    }
}

/// Builds a provider from parts. At least one of `api`/`images`/`classifiers` is required.
pub fn create_provider(input: CreateProviderOptions) -> anyhow::Result<Arc<dyn Provider>> {
    let stream_count = match &input.api {
        Some(ProviderApi::Single(_)) => 1,
        Some(ProviderApi::ByApi(map)) => map.len(),
        None => 0,
    };
    if stream_count == 0 && input.images.is_empty() && input.classifiers.is_empty() {
        anyhow::bail!("Provider {}: at least one of \"api\", \"images\", or \"classifiers\" is required.", input.id);
    }
    Ok(Arc::new(DefinedProvider {
        name: input.name.clone().unwrap_or_else(|| input.id.clone()),
        id: input.id,
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        baseline: input.models,
        dynamic: Arc::new(RwLock::new(Vec::new())),
        fetch_models: input.fetch_models,
        filter_models_fn: input.filter_models,
        filter_all_models_fn: input.filter_all_models,
        api: input.api,
        images: input.images,
        classifiers: input.classifiers,
    }))
}

// ---------------------------------------------------------------------------
// Models collection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ModelsRefreshOptions {
    pub allow_network: Option<bool>,
    /// Restrict refresh to these provider IDs.
    pub providers: Option<Vec<String>>,
    pub force: Option<bool>,
    pub signal: Option<CancellationToken>,
}

#[derive(Debug, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    /// Errors by provider ID, in the order the refreshes failed (TS: a `Map`).
    pub errors: IndexMap<String, String>,
}

#[derive(Clone, Default)]
pub struct CreateModelsOptions {
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub models_store: Option<Arc<dyn ModelsStore>>,
    pub auth_context: Option<Arc<dyn AuthContext>>,
    /// Default identity of every request.
    pub identity: Option<ClientIdentity>,
}

/// Transform of fully assembled request headers (`ModelsRequestTransforms.transformHeaders`).
pub type TransformHeadersFn = Arc<dyn Fn(ProviderHeaders) -> BoxFuture<'static, ProviderHeaders> + Send + Sync>;

/// Options for `Models` request methods: provider options plus Models-only transforms.
#[derive(Clone, Default)]
pub struct ModelsRequestOptions {
    pub options: StreamOptions,
    pub transform_headers: Option<TransformHeadersFn>,
}

impl From<StreamOptions> for ModelsRequestOptions {
    fn from(options: StreamOptions) -> Self {
        Self { options, transform_headers: None }
    }
}

struct ModelsInner {
    providers: RwLock<IndexMap<String, Arc<dyn Provider>>>,
    credentials: Arc<dyn CredentialStore>,
    models_store: Arc<dyn ModelsStore>,
    auth_context: Arc<dyn AuthContext>,
    refresh_generations: Mutex<HashMap<String, u64>>,
    refresh_controllers: Mutex<HashMap<String, (u64, CancellationToken)>>,
    publication_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Runtime collection of providers plus auth application and request convenience
/// (`MutableModels`). Cheap to clone; clones share providers.
#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
    identity: Option<ClientIdentity>,
}

pub type MutableModels = Models;

pub use crate::telemetry::AI_TELEMETRY_SCHEMA;

/// Merge headers case-insensitively; override entries replace base entries.
pub fn merge_headers(base: Option<&ProviderHeaders>, override_: Option<&ProviderHeaders>) -> Option<ProviderHeaders> {
    if base.is_none() && override_.is_none() {
        return None;
    }
    let mut merged = base.cloned().unwrap_or_default();
    if let Some(override_) = override_ {
        for (name, value) in override_ {
            let lower = name.to_lowercase();
            merged.retain(|existing, _| existing.to_lowercase() != lower);
            merged.insert(name.clone(), value.clone());
        }
    }
    Some(merged)
}

fn string_headers(headers: &IndexMap<String, String>) -> ProviderHeaders {
    headers.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect()
}

/// Create a `Models` collection.
pub fn create_models(options: CreateModelsOptions) -> Models {
    Models {
        inner: Arc::new(ModelsInner {
            providers: RwLock::new(IndexMap::new()),
            credentials: options.credentials.unwrap_or_else(|| Arc::new(InMemoryCredentialStore::new())),
            models_store: options.models_store.unwrap_or_else(|| Arc::new(InMemoryModelsStore::new())),
            auth_context: options.auth_context.unwrap_or_else(default_provider_auth_context),
            refresh_generations: Mutex::new(HashMap::new()),
            refresh_controllers: Mutex::new(HashMap::new()),
            publication_locks: Mutex::new(HashMap::new()),
        }),
        identity: options.identity,
    }
}

impl Default for Models {
    fn default() -> Self {
        create_models(CreateModelsOptions::default())
    }
}

impl Models {
    /// This collection with `identity` as the default identity of its requests
    /// (`withClientIdentity`). Providers are shared.
    pub fn with_client_identity(&self, identity: ClientIdentity) -> Models {
        Models {
            inner: self.inner.clone(),
            identity: merge_client_identity(&[self.identity.as_ref(), Some(&identity)]),
        }
    }

    pub fn credentials(&self) -> &Arc<dyn CredentialStore> {
        &self.inner.credentials
    }

    pub fn auth_context(&self) -> &Arc<dyn AuthContext> {
        &self.inner.auth_context
    }

    // ---- provider registry ----

    /// Upsert/replace by provider id.
    pub fn set_provider(&self, provider: Arc<dyn Provider>) {
        let id = provider.id().to_string();
        self.supersede_provider_refresh(&id);
        self.inner.providers.write().insert(id, provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.supersede_provider_refresh(id);
        self.inner.providers.write().shift_remove(id);
    }

    pub fn clear_providers(&self) {
        let mut ids: Vec<String> = self.inner.providers.read().keys().cloned().collect();
        ids.extend(self.inner.refresh_controllers.lock().keys().cloned());
        ids.sort();
        ids.dedup();
        for id in ids {
            self.supersede_provider_refresh(&id);
        }
        self.inner.providers.write().clear();
    }

    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.inner.providers.read().values().cloned().collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.inner.providers.read().get(id).cloned()
    }

    // ---- model reads ----

    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        match provider {
            Some(id) => self.get_provider(id).map(|p| p.get_models()).unwrap_or_default(),
            None => self.get_providers().iter().flat_map(|p| p.get_models()).collect(),
        }
    }

    pub fn get_all_models(&self, provider: Option<&str>) -> Vec<AnyModel> {
        match provider {
            Some(id) => self.get_provider(id).map(|p| p.get_all_models()).unwrap_or_default(),
            None => self.get_providers().iter().flat_map(|p| p.get_all_models()).collect(),
        }
    }

    pub fn get_models_of_type(&self, model_type: ModelType, provider: Option<&str>) -> Vec<AnyModel> {
        self.get_all_models(provider).into_iter().filter(|m| m.model_type() == model_type).collect()
    }

    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider)).into_iter().find(|m| m.id == id)
    }

    pub fn get_model_of_type(&self, model_type: ModelType, provider: &str, id: &str) -> Option<AnyModel> {
        self.get_models_of_type(model_type, Some(provider)).into_iter().find(|m| m.id() == id)
    }

    // ---- refresh ----

    fn supersede_provider_refresh(&self, provider_id: &str) -> u64 {
        let generation = {
            let mut generations = self.inner.refresh_generations.lock();
            let next = generations.get(provider_id).copied().unwrap_or(0) + 1;
            generations.insert(provider_id.to_string(), next);
            next
        };
        if let Some((_, previous)) = self.inner.refresh_controllers.lock().remove(provider_id) {
            previous.cancel();
        }
        generation
    }

    fn begin_provider_refresh(&self, provider_id: &str) -> (u64, CancellationToken) {
        let generation = self.supersede_provider_refresh(provider_id);
        let controller = CancellationToken::new();
        self.inner.refresh_controllers.lock().insert(provider_id.to_string(), (generation, controller.clone()));
        (generation, controller)
    }

    fn publish_fn(&self, provider_id: String, generation: u64, signal: CancellationToken) -> PublishFn {
        let inner = self.inner.clone();
        Arc::new(move |publication: ModelsPublication| {
            let inner = inner.clone();
            let provider_id = provider_id.clone();
            let signal = signal.clone();
            Box::pin(async move {
                let lock = inner.publication_locks.lock().entry(provider_id.clone()).or_default().clone();
                let operation = async {
                    let _guard = lock.lock().await;
                    let current = || inner.refresh_generations.lock().get(&provider_id).copied();
                    if signal.is_cancelled() || current() != Some(generation) {
                        return Ok(false);
                    }
                    match publication.persist {
                        Some(None) => inner.models_store.delete(&provider_id, Some(&signal)).await?,
                        Some(Some(entry)) => inner.models_store.write(&provider_id, entry, Some(&signal)).await?,
                        None => {}
                    }
                    if signal.is_cancelled() || current() != Some(generation) {
                        return Ok(false);
                    }
                    if let Some(update) = publication.update {
                        update();
                    }
                    Ok(true)
                };
                race_with_signal(operation, &signal).await
            })
        })
    }

    async fn read_credential(&self, provider_id: &str, signal: &CancellationToken) -> anyhow::Result<Option<Credential>> {
        self.inner.credentials.read(provider_id, Some(signal)).await.map_err(|error| {
            ModelsError::with_cause(ModelsErrorCode::Auth, format!("Credential store read failed for {provider_id}"), &error)
                .into()
        })
    }

    async fn run_provider_refresh_phase(
        &self,
        provider: &Arc<dyn Provider>,
        credential: Option<Credential>,
        allow_network: bool,
        force: Option<bool>,
        generation: u64,
        signal: &CancellationToken,
    ) -> anyhow::Result<()> {
        let stored = self.inner.models_store.read(provider.id(), Some(signal)).await?;
        let stored = stored.map(|mut entry| {
            entry.models.retain(has_known_model_type);
            entry
        });
        provider
            .refresh_models(RefreshModelsContext {
                credential,
                stored,
                publish: self.publish_fn(provider.id().to_string(), generation, signal.clone()),
                allow_network,
                force: if allow_network { force } else { None },
                signal: signal.clone(),
                base_url: provider.base_url().map(str::to_string),
            })
            .await
    }

    async fn resolve_refresh_credential(
        &self,
        provider: &Arc<dyn Provider>,
        stored: Option<Credential>,
        signal: &CancellationToken,
    ) -> anyhow::Result<Option<Credential>> {
        if let Some(Credential::OAuth(oauth_cred)) = &stored {
            let Some(oauth) = &provider.auth().oauth else { return Ok(None) };
            if crate::utils::now_ms() < oauth_cred.expires {
                return Ok(stored);
            }
            if signal.is_cancelled() {
                return Ok(None);
            }
            let refreshed = refresh_stored_oauth_credential(
                &self.inner.credentials,
                provider.id(),
                oauth,
                Arc::new(|current| crate::utils::now_ms() >= current.expires),
                signal,
            )
            .await?;
            return Ok(refreshed.map(Credential::OAuth));
        }
        let Some(api_key) = &provider.auth().api_key else { return Ok(None) };
        let credential = stored.and_then(|c| c.as_api_key().cloned());
        let result = api_key
            .resolve(ApiKeyResolveInput { ctx: self.inner.auth_context.clone(), credential, signal: signal.clone() })
            .await?;
        Ok(result.map(|r| Credential::ApiKey(ApiKeyCredential { key: r.auth.api_key, env: r.env })))
    }

    /// Refresh selected dynamic providers concurrently.
    pub async fn refresh(&self, options: ModelsRefreshOptions) -> ModelsRefreshResult {
        let allow_network = options.allow_network.unwrap_or(true);
        let caller_signal = options.signal.clone().unwrap_or_default();
        let errors: Arc<Mutex<IndexMap<String, String>>> = Arc::new(Mutex::new(IndexMap::new()));
        if caller_signal.is_cancelled() {
            return ModelsRefreshResult { aborted: true, errors: IndexMap::new() };
        }
        let refreshable: Vec<Arc<dyn Provider>> = self
            .get_providers()
            .into_iter()
            .filter(|p| p.supports_refresh())
            .filter(|p| options.providers.as_ref().is_none_or(|selected| selected.iter().any(|s| s == p.id())))
            .collect();

        let tasks = refreshable.into_iter().map(|provider| {
            let errors = errors.clone();
            let caller_signal = caller_signal.clone();
            let force = options.force;
            async move {
                let (generation, controller) = self.begin_provider_refresh(provider.id());
                let signal = CancellationToken::new();
                let combined = signal.clone();
                let watcher = {
                    let caller = caller_signal.clone();
                    let controller = controller.clone();
                    crate::utils::wasm::spawn(async move {
                        tokio::select! {
                            _ = caller.cancelled() => {},
                            _ = controller.cancelled() => {},
                        }
                        combined.cancel();
                    })
                };
                let operation = async {
                    let (stored, credential_error) = match self.read_credential(provider.id(), &signal).await {
                        Ok(c) => (c, None),
                        Err(e) => (None, Some(e)),
                    };
                    self.run_provider_refresh_phase(&provider, stored.clone(), false, None, generation, &signal).await?;
                    if let Some(error) = credential_error {
                        return Err(error);
                    }
                    if !allow_network || signal.is_cancelled() {
                        return Ok(());
                    }
                    let credential = self.resolve_refresh_credential(&provider, stored, &signal).await?;
                    if credential.is_none() {
                        return Ok(());
                    }
                    self.run_provider_refresh_phase(&provider, credential, true, force, generation, &signal).await
                };
                if let Err(error) = race_with_signal(operation, &signal).await
                    && !signal.is_cancelled() {
                        errors.lock().insert(provider.id().to_string(), error.to_string());
                    }
                watcher.abort();
                let mut controllers = self.inner.refresh_controllers.lock();
                if controllers.get(provider.id()).is_some_and(|(g, _)| *g == generation) {
                    controllers.remove(provider.id());
                }
            }
        });
        let all = futures::future::join_all(tasks);
        tokio::select! {
            _ = all => {},
            _ = caller_signal.cancelled() => {},
        }
        let errors = errors.lock().clone();
        ModelsRefreshResult { aborted: caller_signal.is_cancelled(), errors }
    }

    // ---- auth ----

    async fn check_provider_auth(
        &self,
        provider: &Arc<dyn Provider>,
        credential: Option<&Credential>,
        signal: &CancellationToken,
    ) -> anyhow::Result<Option<AuthCheck>> {
        if let Some(Credential::OAuth(_)) = credential {
            return Ok(provider
                .auth()
                .oauth
                .as_ref()
                .map(|_| AuthCheck { source: Some("OAuth".to_string()), auth_type: AuthType::OAuth }));
        }
        let Some(api_key) = &provider.auth().api_key else { return Ok(None) };
        if api_key.supports_check() {
            return api_key
                .check(ApiKeyResolveInput {
                    ctx: self.inner.auth_context.clone(),
                    credential: credential.and_then(|c| c.as_api_key().cloned()),
                    signal: signal.clone(),
                })
                .await
                .map_err(|error| {
                    ModelsError::with_cause(
                        ModelsErrorCode::Auth,
                        format!("API key auth check failed for provider {}", provider.id()),
                        &error,
                    )
                    .into()
                });
        }
        let resolution = resolve_provider_auth(
            provider.id(),
            provider.auth(),
            &self.inner.credentials,
            &self.inner.auth_context,
            &AuthResolutionOverrides { signal: Some(signal.clone()), ..Default::default() },
        )
        .await?;
        Ok(resolution.map(|r| AuthCheck { source: r.source, auth_type: AuthType::ApiKey }))
    }

    /// Check whether a provider has complete auth configuration without refreshing OAuth.
    pub async fn check_auth(&self, provider_id: &str, signal: Option<CancellationToken>) -> anyhow::Result<Option<AuthCheck>> {
        let signal = signal.unwrap_or_default();
        let operation = async {
            let Some(provider) = self.get_provider(provider_id) else { return Ok(None) };
            let credential = self.read_credential(provider_id, &signal).await?;
            self.check_provider_auth(&provider, credential.as_ref(), &signal).await
        };
        race_with_signal(operation, &signal).await
    }

    async fn get_authenticated_providers(
        &self,
        provider_id: Option<&str>,
        signal: &CancellationToken,
    ) -> anyhow::Result<Vec<(Arc<dyn Provider>, Option<Credential>)>> {
        if signal.is_cancelled() {
            return Err(SignalAbortError.into());
        }
        let providers: Vec<Arc<dyn Provider>> = match provider_id {
            Some(id) => self.get_provider(id).into_iter().collect(),
            None => self.get_providers(),
        };
        let checks = futures::future::join_all(providers.into_iter().map(|provider| async move {
            let credential = self.read_credential(provider.id(), signal).await?;
            let auth = self.check_provider_auth(&provider, credential.as_ref(), signal).await?;
            anyhow::Ok((provider, credential, auth))
        }))
        .await;
        let mut result = Vec::new();
        for check in checks {
            let (provider, credential, auth) = check?;
            if auth.is_some() {
                result.push((provider, credential));
            }
        }
        Ok(result)
    }

    /// Chat models whose providers have complete auth configuration.
    pub async fn get_available(&self, provider_id: Option<&str>, signal: Option<CancellationToken>) -> anyhow::Result<Vec<Model>> {
        let signal = signal.unwrap_or_default();
        let operation = async {
            let providers = self.get_authenticated_providers(provider_id, &signal).await?;
            Ok(providers
                .into_iter()
                .flat_map(|(provider, credential)| {
                    let models = provider.get_models();
                    provider.filter_models(&models, credential.as_ref()).unwrap_or(models)
                })
                .collect())
        };
        race_with_signal(operation, &signal).await
    }

    /// Models of one type whose providers have complete auth configuration.
    pub async fn get_available_of_type(
        &self,
        model_type: ModelType,
        provider_id: Option<&str>,
        signal: Option<CancellationToken>,
    ) -> anyhow::Result<Vec<AnyModel>> {
        Ok(self
            .get_all_available(provider_id, signal)
            .await?
            .into_iter()
            .filter(|m| m.model_type() == model_type)
            .collect())
    }

    /// Models of every type whose providers have complete auth configuration.
    pub async fn get_all_available(&self, provider_id: Option<&str>, signal: Option<CancellationToken>) -> anyhow::Result<Vec<AnyModel>> {
        let signal = signal.unwrap_or_default();
        let operation = async {
            let providers = self.get_authenticated_providers(provider_id, &signal).await?;
            Ok(providers
                .into_iter()
                .flat_map(|(provider, credential)| {
                    let models = provider.get_all_models();
                    if let Some(filtered) = provider.filter_all_models(&models, credential.as_ref()) {
                        return filtered;
                    }
                    let chat = provider.get_models();
                    match provider.filter_models(&chat, credential.as_ref()) {
                        None => models,
                        Some(available) => {
                            let ids: std::collections::HashSet<String> = available.into_iter().map(|m| m.id).collect();
                            models
                                .into_iter()
                                .filter(|m| m.model_type() != ModelType::Chat || ids.contains(m.id()))
                                .collect()
                        }
                    }
                })
                .collect())
        };
        race_with_signal(operation, &signal).await
    }

    /// Resolve provider-scoped auth by provider id. `None` when unknown or unconfigured.
    pub async fn get_auth(&self, provider_id: &str, overrides: AuthResolutionOverrides) -> anyhow::Result<Option<AuthResult>> {
        let Some(provider) = self.get_provider(provider_id) else { return Ok(None) };
        let overrides = AuthResolutionOverrides { signal: Some(overrides.signal.clone().unwrap_or_default()), ..overrides };
        resolve_provider_auth(provider.id(), provider.auth(), &self.inner.credentials, &self.inner.auth_context, &overrides)
            .await
    }

    /// Resolve auth for a model: provider auth plus the model's static headers.
    pub async fn get_auth_for_model(
        &self,
        model: &AnyModel,
        overrides: AuthResolutionOverrides,
    ) -> anyhow::Result<Option<AuthResult>> {
        let result = self.get_auth(model.provider(), overrides).await?;
        let Some(mut result) = result else { return Ok(None) };
        if let Some(headers) = model.headers() {
            result.auth.headers = merge_headers(result.auth.headers.as_ref(), Some(&string_headers(headers)));
        }
        Ok(Some(result))
    }

    /// Run a provider-owned login flow and persist its returned credential.
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: AuthType,
        interaction: Arc<dyn AuthInteraction>,
        signal: Option<CancellationToken>,
        options: Option<LoginOptions>,
    ) -> anyhow::Result<Credential> {
        let signal = signal.unwrap_or_default();
        if signal.is_cancelled() {
            return Err(SignalAbortError.into());
        }
        let provider = self
            .get_provider(provider_id)
            .ok_or_else(|| ModelsError::new(ModelsErrorCode::Provider, format!("Unknown provider: {provider_id}")))?;
        let provider_interaction = ProviderAuthInteraction { interaction, signal: signal.clone() };
        let credential = match auth_type {
            AuthType::OAuth => {
                let oauth = provider.auth().oauth.clone().ok_or_else(|| {
                    ModelsError::new(ModelsErrorCode::Auth, format!("{} does not support oauth login", provider.name()))
                })?;
                Credential::OAuth(race_with_signal(oauth.login(provider_interaction, options), &signal).await?)
            }
            AuthType::ApiKey => {
                let api_key = provider.auth().api_key.clone().filter(|a| a.supports_login()).ok_or_else(|| {
                    ModelsError::new(ModelsErrorCode::Auth, format!("{} does not support api_key login", provider.name()))
                })?;
                Credential::ApiKey(race_with_signal(api_key.login(provider_interaction), &signal).await?)
            }
        };
        let stored = credential.clone();
        let result = self
            .inner
            .credentials
            .modify(provider_id, Box::new(move |_| Box::pin(async move { Ok(Some(stored)) })), Some(&signal))
            .await;
        if let Err(error) = result {
            if signal.is_cancelled() {
                return Err(SignalAbortError.into());
            }
            return Err(ModelsError::with_cause(
                ModelsErrorCode::Auth,
                format!("Credential store modify failed for {provider_id}"),
                &error,
            )
            .into());
        }
        Ok(credential)
    }

    /// Remove the stored credential for a provider.
    pub async fn logout(&self, provider_id: &str, signal: Option<CancellationToken>) -> anyhow::Result<()> {
        let signal = signal.unwrap_or_default();
        if signal.is_cancelled() {
            return Err(SignalAbortError.into());
        }
        self.inner.credentials.delete(provider_id, Some(&signal)).await.map_err(|error| {
            if signal.is_cancelled() {
                return SignalAbortError.into();
            }
            ModelsError::with_cause(ModelsErrorCode::Auth, format!("Credential store delete failed for {provider_id}"), &error)
                .into()
        })
    }

    // ---- requests ----

    fn require_provider(&self, provider_id: &str) -> Result<Arc<dyn Provider>, ModelsError> {
        self.get_provider(provider_id)
            .ok_or_else(|| ModelsError::new(ModelsErrorCode::Provider, format!("Unknown provider: {provider_id}")))
    }

    /// Resolve auth and assemble the provider request options (`applyAuth`).
    pub async fn apply_auth(&self, model: &AnyModel, request: ModelsRequestOptions) -> anyhow::Result<(AnyModel, StreamOptions)> {
        self.require_provider(model.provider())?;
        let options = request.options;
        let resolution = self
            .get_auth_for_model(
                model,
                AuthResolutionOverrides {
                    api_key: options.api_key.clone(),
                    env: options.env.clone(),
                    signal: options.signal.clone(),
                    ..Default::default()
                },
            )
            .await?
            .ok_or_else(|| {
                ModelsError::new(ModelsErrorCode::Auth, format!("Provider is not configured: {}", model.provider()))
            })?;
        let auth = resolution.auth;
        let api_key = options.api_key.clone().or(auth.api_key.clone());
        let mut request_model = model.clone();
        if let Some(base_url) = auth.base_url.clone().filter(|b| !b.is_empty()) {
            request_model.set_base_url(base_url);
        }
        let session_headers = get_open_code_session_headers(
            ClientIdentityTarget { provider: request_model.provider(), base_url: request_model.base_url() },
            options.session_id.as_deref(),
        )
        .map(|h| string_headers(&h));
        let mut headers = merge_headers(
            merge_headers(session_headers.as_ref(), auth.headers.as_ref()).as_ref(),
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
        let identity = merge_client_identity(&[self.identity.as_ref(), options.identity.as_ref()]);
        let mut request_options = options;
        request_options.api_key = api_key;
        request_options.headers = headers;
        request_options.env = env;
        if identity.is_some() {
            request_options.identity = identity;
        }
        Ok((request_model, request_options))
    }

    async fn prepare_chat(
        &self,
        model: &Model,
        request: ModelsRequestOptions,
    ) -> anyhow::Result<(Arc<dyn Provider>, Model, StreamOptions)> {
        assert_chat_model(model)?;
        let provider = self.require_provider(&model.provider)?;
        let (request_model, options) = self.apply_auth(&AnyModel::Chat(model.clone()), request).await?;
        let request_model = request_model.into_chat().expect("chat model stays chat");
        Ok((provider, request_model, options))
    }

    /// Stream a response. Must be called within a tokio runtime.
    pub fn stream(&self, model: &Model, context: &Context, options: impl Into<ModelsRequestOptions>) -> AssistantMessageEventStream {
        let transcript = normalize_context(context);
        let this = self.clone();
        let request: ModelsRequestOptions = options.into();
        let observer = observe_model_request(model, Some(&request.options), false);
        let owned_model = model.clone();
        lazy_stream(
            model,
            Box::pin(async move {
                let (provider, request_model, options) = this.prepare_chat(&owned_model, request).await?;
                Ok(provider.stream(&request_model, &transcript, options))
            }),
            Some(observer),
        )
    }

    pub async fn complete(&self, model: &Model, context: &Context, options: impl Into<ModelsRequestOptions>) -> AssistantMessage {
        self.stream(model, context, options).result().await
    }

    /// Stream with provider-neutral reasoning options.
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: impl Into<ModelsRequestOptions>,
    ) -> AssistantMessageEventStream {
        let transcript = normalize_context(context);
        let this = self.clone();
        let request: ModelsRequestOptions = options.into();
        let observer = observe_model_request(model, Some(&request.options), false);
        let owned_model = model.clone();
        lazy_stream(
            model,
            Box::pin(async move {
                let (provider, request_model, options) = this.prepare_chat(&owned_model, request).await?;
                Ok(provider.stream_simple(&request_model, &transcript, options))
            }),
            Some(observer),
        )
    }

    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: impl Into<ModelsRequestOptions>,
    ) -> AssistantMessage {
        self.stream_simple(model, context, options).result().await
    }

    pub fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: impl Into<ModelsRequestOptions>,
    ) -> AssistantMessageEventStream {
        let this = self.clone();
        let request: ModelsRequestOptions = options.into();
        let observer = observe_model_request(model, Some(&request.options), true);
        let owned_model = model.clone();
        let handle = handle.clone();
        lazy_stream(
            model,
            Box::pin(async move {
                assert_chat_model(&owned_model)?;
                let provider = this.require_provider(&owned_model.provider)?;
                if !provider.supports_fetch_deferred() {
                    return Err(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!("Provider {} does not support deferred responses", owned_model.provider),
                    )
                    .into());
                }
                let (_, request_model, options) = this.prepare_chat(&owned_model, request).await?;
                Ok(provider.fetch_deferred(&request_model, &handle, options))
            }),
            Some(observer),
        )
    }

    pub async fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: impl Into<ModelsRequestOptions>,
    ) -> AssistantMessage {
        self.stream_deferred(model, handle, options).result().await
    }

    pub async fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: impl Into<ModelsRequestOptions>,
    ) -> anyhow::Result<()> {
        assert_chat_model(model)?;
        let provider = self.require_provider(&model.provider)?;
        if !provider.supports_cancel_deferred() {
            return Err(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Provider {} does not support deferred responses", model.provider),
            )
            .into());
        }
        let (_, request_model, options) = self.prepare_chat(model, options.into()).await?;
        provider.cancel_deferred(&request_model, handle, options).await
    }

    /// Generate images through the owning provider. Never fails: errors become results.
    pub async fn generate_images(
        &self,
        model: &ImageModel,
        context: &ImagesContext,
        options: impl Into<ModelsRequestOptions>,
    ) -> AssistantImages {
        let request: ModelsRequestOptions = options.into();
        let aborted = request.options.signal.clone();
        let result: anyhow::Result<AssistantImages> = async {
            assert_image_model(model)?;
            let provider = self.require_provider(&model.provider)?;
            if !provider.supports_images() {
                return Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} does not support image generation", model.provider),
                )
                .into());
            }
            let (request_model, options) = self.apply_auth(&AnyModel::Image(model.clone()), request).await?;
            let AnyModel::Image(request_model) = request_model else { unreachable!() };
            Ok(provider.generate_images(&request_model, context, options).await)
        }
        .await;
        match result {
            Ok(images) => images,
            Err(error) => image_error_result(model, &error, aborted.is_some_and(|s| s.is_cancelled())),
        }
    }

    /// Classify structured state through the owning provider. Never fails.
    pub async fn classify(
        &self,
        model: &ClassifierModel,
        context: &ClassifierContext,
        options: impl Into<ModelsRequestOptions>,
    ) -> ClassifierResult {
        let request: ModelsRequestOptions = options.into();
        let aborted = request.options.signal.clone();
        let result: anyhow::Result<ClassifierResult> = async {
            assert_classifier_model(model)?;
            assert_classifier_input_supported(model, context)?;
            let provider = self.require_provider(&model.provider)?;
            if !provider.supports_classify() {
                return Err(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Provider {} does not support classification", model.provider),
                )
                .into());
            }
            let (request_model, options) = self.apply_auth(&AnyModel::Classifier(model.clone()), request).await?;
            let AnyModel::Classifier(request_model) = request_model else { unreachable!() };
            Ok(provider.classify(&request_model, context, options).await)
        }
        .await;
        match result {
            Ok(result) => result,
            Err(error) => classifier_error_result(model, &error, aborted.is_some_and(|s| s.is_cancelled())),
        }
    }
}

// ---------------------------------------------------------------------------
// Model helpers
// ---------------------------------------------------------------------------

/// Runtime check that a model is a chat model using `api`.
pub fn has_api(model: &AnyModel, api: &str) -> bool {
    matches!(model, AnyModel::Chat(m) if m.api == api)
}

/// Compute and store usage cost from model pricing (`calculateCost`).
pub fn calculate_cost(cost: &ModelCost, usage: &mut Usage) -> UsageCost {
    let input_tokens = (usage.input + usage.cache_read + usage.cache_write) as f64;
    let mut rates = cost.rates();
    let mut matched_threshold = -1.0;
    for tier in cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above && tier.input_tokens_above > matched_threshold {
            rates = tier.rates();
            matched_threshold = tier.input_tokens_above;
        }
    }
    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    usage.cost.input = (rates.input / 1_000_000.0) * usage.input as f64;
    usage.cost.output = (rates.output / 1_000_000.0) * usage.output as f64;
    usage.cost.cache_read = (rates.cache_read / 1_000_000.0) * usage.cache_read as f64;
    usage.cost.cache_write = (rates.cache_write * short_write + rates.input * 2.0 * long_write) / 1_000_000.0;
    usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.clone()
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = crate::providers::model_schema::MODEL_THINKING_LEVELS;

/// Thinking levels a model supports.
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(level));
            match mapped {
                Some(None) => false,
                Some(Some(_)) => true,
                None => !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max),
            }
        })
        .collect()
}

/// Clamp a requested thinking level to the nearest supported level (upward first).
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let Some(requested) = EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) else {
        return available.first().copied().unwrap_or(ModelThinkingLevel::Off);
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available.first().copied().unwrap_or(ModelThinkingLevel::Off)
}

/// Whether two models are equal by type, id, and provider.
pub fn models_are_equal(a: Option<&AnyModel>, b: Option<&AnyModel>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.model_type() == b.model_type() && a.id() == b.id() && a.provider() == b.provider(),
        _ => false,
    }
}

/// [`models_are_equal`] for chat models.
pub fn chat_models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}
