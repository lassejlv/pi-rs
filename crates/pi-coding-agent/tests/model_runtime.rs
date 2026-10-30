//! Ports of the `ModelRuntime` tests: `test/model-runtime-auth-options.test.ts`,
//! `test/model-runtime-credential-sync.test.ts`, `test/model-runtime-classifiers.test.ts`,
//! `test/model-runtime-images.test.ts`, `test/model-runtime-modify-models-compat.test.ts` and
//! `test/model-runtime-cloudflare-compat.test.ts`.
//!
//! TS `fetch` mocks become a local HTTP server (pi-ai's `mock_http`) reached through the model's
//! base URL; requests to fixed `https://` endpoints are checked through the resolved request auth
//! (base URL and headers) instead.

#[path = "../../pi-ai/tests/mock_http/mod.rs"]
mod mock_http;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::auth::credential_store::InMemoryCredentialStore;
use pi_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthCheck, AuthEvent, AuthInteraction, AuthPrompt, AuthResult,
    AuthType, Credential, CredentialInfo, CredentialStore, LoginOptions, ModelAuth, ModifyCredentialFn, OAuthAuth,
    OAuthCredential, OAuthCredentials, ProviderAuth, ProviderAuthInteraction,
};
use pi_ai::*;
use pi_coding_agent::core::auth_storage::AuthStorage;
use pi_coding_agent::core::extensions::types::{
    ProviderChatModelConfig, ProviderConfig, ProviderModelConfig, ProviderOAuthConfig,
};
use pi_coding_agent::core::model_registry::{ModelRegistry, ResolvedRequestAuth};
use pi_coding_agent::core::model_runtime::{
    CreateModelRuntimeOptions, CredentialSynchronizationError, ModelRuntime, ModelRuntimeAuthOverrides,
    ModelRuntimeGetAuthProviderOrModel,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn oauth(access: &str, refresh: &str, expires: i64) -> Credential {
    Credential::OAuth(OAuthCredentials {
        access: access.into(),
        refresh: refresh.into(),
        expires,
        extra: Default::default(),
    })
}

fn api_key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
}

fn auth_storage(entries: &[(&str, Credential)]) -> Arc<AuthStorage> {
    let data: IndexMap<String, Credential> = entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    AuthStorage::in_memory(Some(&data))
}

async fn runtime_with(credentials: Arc<dyn CredentialStore>) -> Arc<ModelRuntime> {
    ModelRuntime::create(Some(&CreateModelRuntimeOptions {
        credentials: Some(credentials),
        models_path: Some(String::new()),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        allow_model_network: Some(false),
        ..Default::default()
    }))
    .await
    .unwrap()
}

async fn runtime_with_models_json(credentials: Arc<dyn CredentialStore>, models_json: Value) -> (Arc<ModelRuntime>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(&path, models_json.to_string()).unwrap();
    let runtime = ModelRuntime::create(Some(&CreateModelRuntimeOptions {
        credentials: Some(credentials),
        models_path: Some(path.to_string_lossy().into_owned()),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        allow_model_network: Some(false),
        ..Default::default()
    }))
    .await
    .unwrap();
    (runtime, dir)
}

fn chat_config(id: &str) -> ProviderModelConfig {
    ProviderModelConfig::Chat(ProviderChatModelConfig {
        id: id.into(),
        name: id.into(),
        api: None,
        base_url: None,
        input: vec![pi_ai::providers::model_schema::ModelInputModality::Text],
        input_limits: None,
        cost: Default::default(),
        headers: None,
        reasoning: false,
        thinking_level_map: None,
        prompt_cache: None,
        context_window: 10000,
        max_tokens: 1000,
        sampling_params: None,
        compat: None,
    })
}

fn model_json(provider: &str, id: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": id, "api": "openai-completions", "provider": provider, "baseUrl": base_url,
        "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

fn image_model(provider: &str, id: &str) -> ImageModel {
    serde_json::from_value(json!({
        "type": "image", "id": id, "name": id, "api": "test-images", "provider": provider,
        "baseUrl": "https://images.test/v1", "input": ["text"], "output": ["image"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
    }))
    .unwrap()
}

fn classifier_model(provider: &str, id: &str) -> ClassifierModel {
    serde_json::from_value(json!({
        "type": "classifier", "id": id, "name": id, "api": "test-classifier", "provider": provider,
        "baseUrl": "https://classifier.test/v1", "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 1000
    }))
    .unwrap()
}

fn images_context() -> ImagesContext {
    serde_json::from_value(json!({ "input": [{ "type": "text", "text": "a red circle" }] })).unwrap()
}

fn classifier_context() -> ClassifierContext {
    serde_json::from_value(json!({
        "state": { "text": "Looks good" },
        "questions": {
            "approved": {
                "type": "bool",
                "instructions": "Does this express approval?",
                "criteria": { "true": "Approval", "false": "No approval" }
            }
        }
    }))
    .unwrap()
}

fn ok_image_result(model: &ImageModel) -> AssistantImages {
    serde_json::from_value(json!({
        "api": model.api, "provider": model.provider, "model": model.id,
        "output": [{ "type": "image", "data": "aGk=", "mimeType": "image/png" }],
        "stopReason": "stop", "timestamp": 0
    }))
    .unwrap()
}

/// Auth interaction answering every prompt with a fixed value.
struct FixedInteraction(String);

#[async_trait]
impl AuthInteraction for FixedInteraction {
    async fn prompt(&self, _prompt: AuthPrompt) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
    fn notify(&self, _event: AuthEvent) {}
}

fn interaction(value: &str) -> Arc<dyn AuthInteraction> {
    Arc::new(FixedInteraction(value.into()))
}

type LoginFn = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<ApiKeyCredential>> + Send + Sync>;
type CheckFn = Arc<dyn Fn(Option<ApiKeyCredential>) -> BoxFuture<'static, anyhow::Result<Option<AuthCheck>>> + Send + Sync>;
type ResolveFn =
    Arc<dyn Fn(Option<ApiKeyCredential>) -> BoxFuture<'static, anyhow::Result<Option<AuthResult>>> + Send + Sync>;

/// Configurable api-key auth (TS object literal `auth.apiKey`).
struct TestApiKeyAuth {
    name: String,
    login: Option<LoginFn>,
    check: Option<CheckFn>,
    resolve: ResolveFn,
}

#[async_trait]
impl ApiKeyAuth for TestApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }
    fn supports_login(&self) -> bool {
        self.login.is_some()
    }
    async fn login(&self, _interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        (self.login.as_ref().expect("login"))().await
    }
    fn supports_check(&self) -> bool {
        self.check.is_some()
    }
    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        match &self.check {
            Some(check) => check(input.credential).await,
            None => Ok(None),
        }
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        (self.resolve)(input.credential).await
    }
}

fn stored_key_resolve() -> ResolveFn {
    Arc::new(|credential: Option<ApiKeyCredential>| {
        async move {
            Ok(credential.and_then(|c| c.key).map(|key| AuthResult {
                auth: ModelAuth { api_key: Some(key), ..Default::default() },
                env: None,
                source: Some("stored".into()),
            }))
        }
        .boxed()
    })
}

fn stored_check() -> CheckFn {
    Arc::new(|credential: Option<ApiKeyCredential>| {
        async move { Ok(credential.map(|_| AuthCheck { source: Some("stored".into()), auth_type: AuthType::ApiKey })) }.boxed()
    })
}

type RefreshFn = Arc<dyn Fn(RefreshModelsContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Hand-written provider (TS object literal `Provider`).
struct TestProvider {
    id: String,
    name: String,
    auth: ProviderAuth,
    models: Vec<Model>,
    refresh: Option<RefreshFn>,
    deferred: Option<Arc<Mutex<DeferredCapture>>>,
}

#[derive(Default)]
struct DeferredCapture {
    fetched_base_url: Option<String>,
    fetched_options: Option<StreamOptions>,
    cancelled_id: Option<String>,
    cancelled_options: Option<StreamOptions>,
}

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn supports_refresh(&self) -> bool {
        self.refresh.is_some()
    }
    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        match &self.refresh {
            Some(refresh) => refresh(context).await,
            None => Ok(()),
        }
    }
    fn stream(&self, _model: &Model, _context: &TranscriptContext, _options: StreamOptions) -> AssistantMessageEventStream {
        panic!("unused")
    }
    fn stream_simple(&self, _model: &Model, _context: &TranscriptContext, _options: StreamOptions) -> AssistantMessageEventStream {
        panic!("unused")
    }
    fn supports_fetch_deferred(&self) -> bool {
        self.deferred.is_some()
    }
    fn fetch_deferred(&self, model: &Model, _handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        let capture = self.deferred.as_ref().unwrap();
        capture.lock().fetched_base_url = Some(model.base_url.clone());
        capture.lock().fetched_options = Some(options);
        let message: AssistantMessage = serde_json::from_value(json!({
            "role": "assistant", "content": [], "api": model.api, "provider": model.provider, "model": model.id,
            "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
            "stopReason": "stop", "timestamp": 0
        }))
        .unwrap();
        let (sender, stream) = AssistantMessageEventStream::new();
        sender.push(AssistantMessageEvent::Start { partial: message.clone() });
        sender.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message: message.clone() });
        sender.end(Some(message));
        stream
    }
    fn supports_cancel_deferred(&self) -> bool {
        self.deferred.is_some()
    }
    async fn cancel_deferred(&self, _model: &Model, handle: &DeferredHandle, options: StreamOptions) -> anyhow::Result<()> {
        let capture = self.deferred.as_ref().unwrap();
        capture.lock().cancelled_id = Some(handle.id.clone());
        capture.lock().cancelled_options = Some(options);
        Ok(())
    }
}

/// TS `provider(id, { login, refreshModels })` of the credential-sync tests.
fn sync_provider(id: &str, login: Option<LoginFn>, check: Option<CheckFn>, refresh: Option<RefreshFn>) -> Arc<TestProvider> {
    let key = format!("{id}-key");
    let login = login.unwrap_or_else(|| {
        Arc::new(move || {
            let key = key.clone();
            async move { Ok(ApiKeyCredential { key: Some(key), env: None }) }.boxed()
        })
    });
    Arc::new(TestProvider {
        id: id.into(),
        name: id.into(),
        auth: ProviderAuth {
            api_key: Some(Arc::new(TestApiKeyAuth {
                name: "API key".into(),
                login: Some(login),
                check: Some(check.unwrap_or_else(stored_check)),
                resolve: stored_key_resolve(),
            })),
            oauth: None,
        },
        models: vec![model_json(id, "dynamic", "https://example.test/v1")],
        refresh,
        deferred: None,
    })
}

async fn runtime_with_provider(provider: Arc<TestProvider>, credentials: Arc<AuthStorage>) -> Arc<ModelRuntime> {
    let runtime = runtime_with(credentials).await;
    let id = provider.id.clone();
    runtime.register_native_provider(provider).unwrap();
    runtime
        .refresh(Some(ModelsRefreshOptions { allow_network: Some(false), providers: Some(vec![id]), ..Default::default() }))
        .await
        .unwrap();
    runtime
}

fn refresh_opts(allow_network: bool, providers: &[&str]) -> Option<ModelsRefreshOptions> {
    Some(ModelsRefreshOptions {
        allow_network: Some(allow_network),
        providers: (!providers.is_empty()).then(|| providers.iter().map(|p| p.to_string()).collect()),
        ..Default::default()
    })
}

/// `(type, provider id, provider name, method name)` for every auth method (TS `authOptions`).
fn auth_options(runtime: &ModelRuntime, filter: Option<AuthType>) -> Vec<(AuthType, String, String, String)> {
    let mut out = Vec::new();
    for provider in runtime.get_providers() {
        if (filter.is_none() || filter == Some(AuthType::OAuth))
            && let Some(oauth) = &provider.auth().oauth
        {
            out.push((AuthType::OAuth, provider.id().to_string(), provider.name().to_string(), oauth.name().to_string()));
        }
        if (filter.is_none() || filter == Some(AuthType::ApiKey))
            && let Some(api_key) = &provider.auth().api_key
        {
            out.push((AuthType::ApiKey, provider.id().to_string(), provider.name().to_string(), api_key.name().to_string()));
        }
    }
    out
}

async fn wait_for(flag: &AtomicBool) {
    for _ in 0..500 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not reached");
}

fn pending<T: Send + 'static>() -> BoxFuture<'static, T> {
    std::future::pending().boxed()
}

// ---------------------------------------------------------------------------
// model-runtime-auth-options.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepts_a_pi_ai_credential_store() {
    let credentials = Arc::new(InMemoryCredentialStore::new());
    credentials
        .modify("anthropic", Box::new(|_| async { Ok(Some(api_key("stored-key"))) }.boxed()), None)
        .await
        .unwrap();
    let runtime = runtime_with(credentials).await;
    let auth = runtime.get_auth(&"anthropic".into(), None).await.unwrap().unwrap();
    assert_eq!(auth.auth.api_key.as_deref(), Some("stored-key"));
}

/// A credential store wrapper that records reads and can fail them.
struct RecordingStore {
    base: InMemoryCredentialStore,
    reads: Mutex<Vec<String>>,
    fail_reads: AtomicBool,
}

#[async_trait]
impl CredentialStore for RecordingStore {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        self.reads.lock().push(provider_id.to_string());
        if self.fail_reads.load(Ordering::SeqCst) {
            anyhow::bail!("read failed for {provider_id}");
        }
        self.base.read(provider_id, signal).await
    }
    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        self.base.list(signal).await
    }
    async fn modify(&self, provider_id: &str, f: ModifyCredentialFn, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        self.base.modify(provider_id, f, signal).await
    }
    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        self.base.delete(provider_id, signal).await
    }
}

#[tokio::test]
async fn scopes_provider_availability_reads_and_records_refresh_failures() {
    let store = Arc::new(RecordingStore {
        base: InMemoryCredentialStore::new(),
        reads: Mutex::new(Vec::new()),
        fail_reads: AtomicBool::new(false),
    });
    let runtime = runtime_with(store.clone()).await;

    store.reads.lock().clear();
    runtime.get_available(Some("anthropic"), None).await.unwrap();
    let reads: std::collections::HashSet<String> = store.reads.lock().iter().cloned().collect();
    assert_eq!(reads, ["anthropic".to_string()].into_iter().collect());

    store.fail_reads.store(true, Ordering::SeqCst);
    let error = runtime.get_available(Some("anthropic"), None).await.unwrap_err();
    assert!(error.to_string().contains("Credential store read failed for anthropic"), "{error}");
    assert!(
        runtime
            .get_error()
            .unwrap_or_default()
            .contains("Availability refresh: Credential store read failed for anthropic"),
        "{:?}",
        runtime.get_error()
    );

    store.fail_reads.store(false, Ordering::SeqCst);
    runtime.get_available(None, None).await.unwrap();
    assert_eq!(runtime.get_error(), None);
}

#[tokio::test]
async fn projects_provider_owned_methods_names_and_status() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let options = auth_options(&runtime, None);
    let has = |t: AuthType, id: &str, name: &str, method: Option<&str>| {
        options
            .iter()
            .any(|(ot, oid, oname, omethod)| *ot == t && oid == id && oname == name && method.is_none_or(|m| m == omethod))
    };
    assert!(has(AuthType::ApiKey, "amazon-bedrock", "Amazon Bedrock", Some("AWS credentials or bearer token")));
    assert!(has(AuthType::ApiKey, "google-vertex", "Google Vertex AI", Some("Google Cloud credentials")));
    assert!(has(AuthType::OAuth, "anthropic", "Anthropic", None));
    assert!(has(AuthType::ApiKey, "cloudflare-ai-gateway", "Cloudflare AI Gateway", None));
    assert!(has(AuthType::ApiKey, "cloudflare-workers-ai", "Cloudflare Workers AI", None));
    assert!(auth_options(&runtime, Some(AuthType::ApiKey)).iter().all(|o| o.0 == AuthType::ApiKey));
    assert!(auth_options(&runtime, Some(AuthType::OAuth)).iter().all(|o| o.0 == AuthType::OAuth));
    assert!(!options.iter().any(|o| o.1 == "openai-codex" && o.0 == AuthType::ApiKey));
}

#[tokio::test]
async fn attaches_the_providers_active_auth_status_to_every_method_option() {
    let runtime = runtime_with(auth_storage(&[("anthropic", oauth("access", "refresh", now_ms() + 60_000))])).await;
    let options: Vec<_> = auth_options(&runtime, None).into_iter().filter(|o| o.1 == "anthropic").collect();
    assert_eq!(options.len(), 2);
    let check = runtime.check_auth("anthropic", None).await.unwrap().unwrap();
    assert_eq!(check.auth_type, AuthType::OAuth);
}

#[tokio::test]
async fn distinguishes_subscription_oauth_from_generic_oauth_sign_in() {
    let runtime = runtime_with(auth_storage(&[
        ("anthropic", oauth("anthropic-access", "anthropic-refresh", now_ms() + 3_600_000)),
        ("openrouter", oauth("openrouter-key", "", 9_007_199_254_740_991)),
        ("radius", oauth("radius-access", "radius-refresh", now_ms() + 3_600_000)),
    ]))
    .await;
    assert!(runtime.is_using_o_auth("anthropic"));
    assert!(runtime.is_using_subscription("anthropic"));
    assert!(runtime.is_using_o_auth("openrouter"));
    assert!(!runtime.is_using_subscription("openrouter"));
    assert!(runtime.is_using_o_auth("radius"));
    assert!(!runtime.is_using_subscription("radius"));
}

#[tokio::test]
async fn constructs_an_api_key_method_for_an_extension_api_key_provider() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime
        .register_provider(
            "extension-api-key",
            &ProviderConfig {
                name: Some("Extension API Key".into()),
                base_url: Some("https://example.test/v1".into()),
                api_key: Some("$EXTENSION_TEST_API_KEY".into()),
                api: Some("openai-completions".into()),
                models: Some(vec![chat_config("extension-model")]),
                ..Default::default()
            },
        )
        .unwrap();
    let options: Vec<_> = auth_options(&runtime, None).into_iter().filter(|o| o.1 == "extension-api-key").collect();
    assert_eq!(
        options,
        vec![(AuthType::ApiKey, "extension-api-key".to_string(), "Extension API Key".to_string(), "API key".to_string())]
    );
    let provider = runtime.get_provider("extension-api-key").unwrap();
    assert!(provider.auth().api_key.as_ref().unwrap().supports_login());
}

#[tokio::test]
async fn resolves_configured_auth_from_request_scoped_environment_overrides() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime
        .register_provider(
            "request-env-provider",
            &ProviderConfig {
                base_url: Some("https://example.test/v1".into()),
                api_key: Some("$REQUEST_SCOPED_API_KEY".into()),
                headers: Some([("x-request-value".to_string(), "$REQUEST_SCOPED_HEADER".to_string())].into_iter().collect()),
                api: Some("openai-completions".into()),
                models: Some(vec![chat_config("request-env-model")]),
                ..Default::default()
            },
        )
        .unwrap();
    let env: IndexMap<String, String> = [
        ("REQUEST_SCOPED_API_KEY".to_string(), "request-key".to_string()),
        ("REQUEST_SCOPED_HEADER".to_string(), "request-header".to_string()),
    ]
    .into_iter()
    .collect();
    let auth = runtime
        .get_auth(&"request-env-provider".into(), Some(&ModelRuntimeAuthOverrides { env: Some(env), ..Default::default() }))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(auth.auth.api_key.as_deref(), Some("request-key"));
    assert_eq!(
        auth.auth.headers,
        Some([("x-request-value".to_string(), Some("request-header".to_string()))].into_iter().collect())
    );
    assert_eq!(auth.auth.base_url, None);
}

type CapturedHeaders = Arc<Mutex<Option<Option<ProviderHeaders>>>>;

fn capturing_stream_simple(captured: CapturedHeaders) -> pi_coding_agent::core::extensions::types::StreamSimpleFn {
    Arc::new(move |model: Model, _context, options: Option<SimpleStreamOptions>| {
        *captured.lock() = Some(options.and_then(|o| o.headers));
        pi_ai::api::lazy::error_stream(&model, anyhow::anyhow!("captured"))
    })
}

fn headers(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect()
}

#[tokio::test]
async fn lets_an_explicit_authorization_header_override_auth_header_case_insensitively() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let captured: CapturedHeaders = Arc::new(Mutex::new(None));
    runtime
        .register_provider(
            "auth-header-provider",
            &ProviderConfig {
                base_url: Some("https://example.test/v1".into()),
                api_key: Some("generated-key".into()),
                auth_header: Some(true),
                api: Some("openai-completions".into()),
                stream_simple: Some(capturing_stream_simple(captured.clone())),
                models: Some(vec![chat_config("auth-header-model")]),
                ..Default::default()
            },
        )
        .unwrap();
    let model = runtime.get_model("auth-header-provider", "auth-header-model").unwrap();
    runtime
        .complete_simple(
            model,
            Context::default(),
            Some(StreamOptions { headers: Some(headers(&[("authorization", "Explicit token")])), ..Default::default() }.into()),
        )
        .await;
    assert_eq!(captured.lock().clone(), Some(Some(headers(&[("authorization", "Explicit token")]))));
}

#[tokio::test]
async fn transforms_fully_assembled_headers_once_without_forwarding_the_transform() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let captured: CapturedHeaders = Arc::new(Mutex::new(None));
    let mut model_config = chat_config("header-model");
    if let ProviderModelConfig::Chat(chat) = &mut model_config {
        chat.headers = Some([("x-model".to_string(), "model".to_string())].into_iter().collect());
    }
    runtime
        .register_provider(
            "header-provider",
            &ProviderConfig {
                base_url: Some("https://example.test/v1".into()),
                api_key: Some("generated-key".into()),
                auth_header: Some(true),
                headers: Some([("x-provider".to_string(), "provider".to_string())].into_iter().collect()),
                api: Some("openai-completions".into()),
                stream_simple: Some(capturing_stream_simple(captured.clone())),
                models: Some(vec![model_config]),
                ..Default::default()
            },
        )
        .unwrap();
    let model = runtime.get_model("header-provider", "header-model").unwrap();
    let transforms = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<Option<ProviderHeaders>>> = Arc::new(Mutex::new(None));
    let (t, s) = (transforms.clone(), seen.clone());
    runtime
        .complete_simple(
            model,
            Context::default(),
            Some(ModelsRequestOptions {
                options: StreamOptions { headers: Some(headers(&[("x-explicit", "explicit")])), ..Default::default() },
                transform_headers: Some(Arc::new(move |mut h: ProviderHeaders| {
                    t.fetch_add(1, Ordering::SeqCst);
                    *s.lock() = Some(h.clone());
                    h.insert("x-transformed".into(), Some("yes".into()));
                    async move { h }.boxed()
                })),
            }),
        )
        .await;
    assert_eq!(transforms.load(Ordering::SeqCst), 1);
    assert_eq!(
        seen.lock().clone(),
        Some(headers(&[
            ("Authorization", "Bearer generated-key"),
            ("x-provider", "provider"),
            ("x-model", "model"),
            ("x-explicit", "explicit"),
        ]))
    );
    assert_eq!(
        captured.lock().clone(),
        Some(Some(headers(&[
            ("Authorization", "Bearer generated-key"),
            ("x-provider", "provider"),
            ("x-model", "model"),
            ("x-explicit", "explicit"),
            ("x-transformed", "yes"),
        ])))
    );
}

fn extension_oauth_config(
    refresh_signal: Option<Arc<Mutex<Option<CancellationToken>>>>,
    is_subscription: Option<bool>,
) -> ProviderOAuthConfig {
    ProviderOAuthConfig {
        name: "Extension subscription".into(),
        is_subscription,
        uses_callback_server: None,
        login: Arc::new(|_| {
            async { Ok(OAuthCredentials { access: "access".into(), refresh: "refresh".into(), expires: now_ms() + 60_000, extra: Default::default() }) }
                .boxed()
        }),
        refresh_token: Arc::new(move |credential, signal| {
            if let Some(slot) = &refresh_signal {
                *slot.lock() = Some(signal);
            }
            async move { Ok(OAuthCredentials { expires: now_ms() + 60_000, ..credential }) }.boxed()
        }),
        get_api_key: Arc::new(|credential| credential.access.clone()),
        modify_models: None,
    }
}

#[tokio::test]
async fn does_not_cancel_an_extension_oauth_refresh_with_the_request() {
    let runtime = runtime_with(auth_storage(&[("extension-oauth", oauth("expired", "refresh", 0))])).await;
    let refresh_signal: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    runtime
        .register_provider(
            "extension-oauth",
            &ProviderConfig {
                name: Some("Extension OAuth".into()),
                base_url: Some("https://example.test/v1".into()),
                api: Some("openai-completions".into()),
                oauth: Some(extension_oauth_config(Some(refresh_signal.clone()), None)),
                models: Some(vec![chat_config("extension-model")]),
                ..Default::default()
            },
        )
        .unwrap();
    let controller = CancellationToken::new();
    runtime
        .get_auth(
            &"extension-oauth".into(),
            Some(&ModelRuntimeAuthOverrides { signal: Some(controller.clone()), ..Default::default() }),
        )
        .await
        .unwrap();
    let signal = refresh_signal.lock().clone().expect("refresh received a signal");
    controller.cancel();
    // A started refresh may already have rotated the refresh token and must be persisted.
    assert!(!signal.is_cancelled());
}

#[tokio::test]
async fn does_not_fabricate_an_api_key_method_for_an_extension_oauth_only_provider() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime
        .register_provider(
            "extension-oauth",
            &ProviderConfig {
                name: Some("Extension OAuth".into()),
                base_url: Some("https://example.test/v1".into()),
                api: Some("openai-completions".into()),
                oauth: Some(extension_oauth_config(None, Some(true))),
                models: Some(vec![chat_config("extension-model")]),
                ..Default::default()
            },
        )
        .unwrap();
    let options: Vec<_> = auth_options(&runtime, None).into_iter().filter(|o| o.1 == "extension-oauth").collect();
    assert_eq!(
        options,
        vec![(AuthType::OAuth, "extension-oauth".to_string(), "Extension OAuth".to_string(), "Extension subscription".to_string())]
    );
    let provider = runtime.get_provider("extension-oauth").unwrap();
    assert_eq!(provider.auth().oauth.as_ref().unwrap().is_subscription(), Some(true));
}

// ---------------------------------------------------------------------------
// model-runtime-credential-sync.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publishes_locally_consistent_availability_before_login_and_logout_resolve() {
    let credentials = AuthStorage::in_memory(None);
    let runtime = runtime_with_provider(sync_provider("dynamic", None, None, None), credentials.clone()).await;

    runtime.login("dynamic", AuthType::ApiKey, interaction("unused"), None, None).await.unwrap();
    assert!(runtime.has_configured_auth("dynamic"));
    assert!(runtime.get_available_snapshot().iter().any(|m| m.id == "dynamic"));
    assert_eq!(credentials.read("dynamic", None).await.unwrap(), Some(api_key("dynamic-key")));

    runtime.logout("dynamic", None).await.unwrap();
    assert!(!runtime.has_configured_auth("dynamic"));
    assert!(!runtime.get_available_snapshot().iter().any(|m| m.provider == "dynamic"));
    assert_eq!(credentials.read("dynamic", None).await.unwrap(), None);
}

/// A login that signals when it starts and waits for `release`.
fn blocking_login(started: Arc<AtomicBool>, release: Arc<tokio::sync::Notify>, key: &str) -> LoginFn {
    let key = key.to_string();
    Arc::new(move || {
        let (started, release, key) = (started.clone(), release.clone(), key.clone());
        async move {
            started.store(true, Ordering::SeqCst);
            release.notified().await;
            Ok(ApiKeyCredential { key: Some(key), env: None })
        }
        .boxed()
    })
}

#[tokio::test]
async fn orders_same_provider_credential_operations_through_local_synchronization() {
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let credentials = AuthStorage::in_memory(None);
    let runtime = runtime_with_provider(
        sync_provider("ordered", Some(blocking_login(started.clone(), release.clone(), "ordered-key")), None, None),
        credentials.clone(),
    )
    .await;

    let login = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.login("ordered", AuthType::ApiKey, interaction("unused"), None, None).await })
    };
    wait_for(&started).await;
    let logout = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.logout("ordered", None).await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(credentials.read("ordered", None).await.unwrap(), None);

    release.notify_one();
    login.await.unwrap().unwrap();
    logout.await.unwrap().unwrap();
    assert_eq!(credentials.read("ordered", None).await.unwrap(), None);
    assert!(!runtime.has_configured_auth("ordered"));
}

#[tokio::test]
async fn allows_different_providers_to_run_credential_flows_concurrently() {
    let (first, second) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime
        .register_native_provider(sync_provider("one", Some(blocking_login(first.clone(), release.clone(), "one")), None, None))
        .unwrap();
    runtime
        .register_native_provider(sync_provider("two", Some(blocking_login(second.clone(), release.clone(), "two")), None, None))
        .unwrap();
    runtime.refresh(refresh_opts(false, &["one", "two"])).await.unwrap();

    let spawn_login = |id: &'static str| {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.login(id, AuthType::ApiKey, interaction("unused"), None, None).await })
    };
    let one = spawn_login("one");
    let two = spawn_login("two");
    wait_for(&first).await;
    wait_for(&second).await;
    release.notify_waiters();
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
}

#[tokio::test]
async fn does_not_wait_for_unrelated_provider_availability_during_local_synchronization() {
    let stall = Arc::new(AtomicBool::new(false));
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime.register_native_provider(sync_provider("target", None, None, None)).unwrap();
    let stall_check = stall.clone();
    let check: CheckFn = Arc::new(move |_| {
        if stall_check.load(Ordering::SeqCst) { pending() } else { async { Ok(None) }.boxed() }
    });
    runtime.register_native_provider(sync_provider("unrelated", None, Some(check), None)).unwrap();
    runtime.refresh(refresh_opts(false, &["target", "unrelated"])).await.unwrap();
    stall.store(true, Ordering::SeqCst);

    tokio::time::timeout(
        Duration::from_secs(5),
        runtime.login("target", AuthType::ApiKey, interaction("unused"), None, None),
    )
    .await
    .expect("login does not wait for unrelated providers")
    .unwrap();
    assert!(runtime.has_configured_auth("target"));
    let result = tokio::time::timeout(Duration::from_secs(5), runtime.refresh(refresh_opts(false, &["target"])))
        .await
        .expect("provider-scoped refresh does not wait for unrelated providers")
        .unwrap();
    assert!(!result.aborted);
}

#[tokio::test]
async fn reports_cancellation_that_occurs_during_provider_scoped_availability() {
    let block = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let (b, s) = (block.clone(), started.clone());
    let check: CheckFn = Arc::new(move |credential| {
        if b.load(Ordering::SeqCst) {
            s.store(true, Ordering::SeqCst);
            return pending();
        }
        async move { Ok(credential.map(|_| AuthCheck { source: Some("stored".into()), auth_type: AuthType::ApiKey })) }.boxed()
    });
    let runtime =
        runtime_with_provider(sync_provider("cancelled-availability", None, Some(check), None), AuthStorage::in_memory(None)).await;
    runtime.set_runtime_api_key("cancelled-availability", "key", None).await.unwrap();
    block.store(true, Ordering::SeqCst);
    let controller = CancellationToken::new();
    let refresh = {
        let (runtime, signal) = (runtime.clone(), controller.clone());
        tokio::spawn(async move {
            runtime
                .refresh(Some(ModelsRefreshOptions {
                    allow_network: Some(false),
                    providers: Some(vec!["cancelled-availability".into()]),
                    signal: Some(signal),
                    ..Default::default()
                }))
                .await
        })
    };
    wait_for(&started).await;
    controller.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), refresh).await.unwrap().unwrap().unwrap();
    assert!(result.aborted);
}

#[tokio::test]
async fn does_not_run_network_refresh_inside_the_credential_operation_chain() {
    let network_calls = Arc::new(AtomicUsize::new(0));
    let calls = network_calls.clone();
    let refresh: RefreshFn = Arc::new(move |context| {
        if context.allow_network {
            calls.fetch_add(1, Ordering::SeqCst);
            return pending();
        }
        async { Ok(()) }.boxed()
    });
    let runtime = runtime_with_provider(sync_provider("local-only", None, None, Some(refresh)), AuthStorage::in_memory(None)).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        runtime.login("local-only", AuthType::ApiKey, interaction("unused"), None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(network_calls.load(Ordering::SeqCst), 0);
    assert!(runtime.has_configured_auth("local-only"));
}

#[tokio::test]
async fn keeps_provider_scoped_refreshes_from_superseding_unrelated_providers() {
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let first_signal: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let (s, r, fs) = (started.clone(), release.clone(), first_signal.clone());
    let refresh: RefreshFn = Arc::new(move |context| {
        if !context.allow_network {
            return async { Ok(()) }.boxed();
        }
        *fs.lock() = Some(context.signal.clone());
        s.store(true, Ordering::SeqCst);
        let r = r.clone();
        async move {
            r.notified().await;
            Ok(())
        }
        .boxed()
    });
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    runtime.register_native_provider(sync_provider("one", None, None, Some(refresh))).unwrap();
    runtime.register_native_provider(sync_provider("two", None, None, None)).unwrap();
    runtime.refresh(refresh_opts(false, &["one", "two"])).await.unwrap();
    runtime.set_runtime_api_key("one", "one-key", None).await.unwrap();
    runtime.set_runtime_api_key("two", "two-key", None).await.unwrap();

    let first = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.refresh(refresh_opts(true, &["one"])).await })
    };
    wait_for(&started).await;
    // TS runs the second refresh interleaved with the first; the Rust runtime serializes
    // refresh() calls, so the second one runs once the first finished. Either way the first
    // provider's network refresh must not be aborted by a refresh of another provider.
    let second = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.refresh(refresh_opts(true, &["two"])).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!first_signal.lock().as_ref().unwrap().is_cancelled());
    release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert!(!first_signal.lock().as_ref().unwrap().is_cancelled());
}

/// Credential store whose `modify` blocks after committing.
struct DelayedCommitStore {
    stored: Mutex<Option<Credential>>,
    committed: AtomicBool,
    finish: tokio::sync::Notify,
}

#[async_trait]
impl CredentialStore for DelayedCommitStore {
    async fn read(&self, _provider_id: &str, _signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        Ok(self.stored.lock().clone())
    }
    async fn list(&self, _signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        Ok(self
            .stored
            .lock()
            .as_ref()
            .map(|c| vec![CredentialInfo { provider_id: "delayed-commit".into(), credential_type: c.auth_type() }])
            .unwrap_or_default())
    }
    async fn modify(&self, _provider_id: &str, f: ModifyCredentialFn, _signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        let current = self.stored.lock().clone();
        if let Some(next) = f(current).await? {
            *self.stored.lock() = Some(next);
        }
        self.committed.store(true, Ordering::SeqCst);
        self.finish.notified().await;
        Ok(self.stored.lock().clone())
    }
    async fn delete(&self, _provider_id: &str, _signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        *self.stored.lock() = None;
        Ok(())
    }
}

fn sync_error(error: &anyhow::Error) -> &CredentialSynchronizationError {
    error
        .downcast_ref::<CredentialSynchronizationError>()
        .unwrap_or_else(|| panic!("expected CredentialSynchronizationError, got {error:#}"))
}

#[tokio::test]
async fn waits_for_a_committed_credential_mutation_to_settle_before_reporting_cancellation() {
    let store = Arc::new(DelayedCommitStore { stored: Mutex::new(None), committed: AtomicBool::new(false), finish: tokio::sync::Notify::new() });
    let runtime = runtime_with(store.clone()).await;
    runtime.register_native_provider(sync_provider("delayed-commit", None, None, None)).unwrap();
    runtime.refresh(refresh_opts(false, &["delayed-commit"])).await.unwrap();
    let controller = CancellationToken::new();
    let login = {
        let (runtime, signal) = (runtime.clone(), controller.clone());
        tokio::spawn(async move {
            runtime.login("delayed-commit", AuthType::ApiKey, interaction("unused"), Some(signal), None).await
        })
    };
    let committed = AtomicBool::new(false);
    for _ in 0..500 {
        if store.committed.load(Ordering::SeqCst) {
            committed.store(true, Ordering::SeqCst);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(committed.load(Ordering::SeqCst));
    controller.cancel();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!login.is_finished());

    store.finish.notify_one();
    let error = login.await.unwrap().unwrap_err();
    let error = sync_error(&error);
    assert_eq!(error.name(), "CredentialSynchronizationError");
    assert_eq!(error.credential(), Some(api_key("delayed-commit-key")));
    assert_eq!(store.stored.lock().clone(), Some(api_key("delayed-commit-key")));
}

#[tokio::test]
async fn reports_a_typed_error_when_cancellation_interrupts_post_commit_synchronization() {
    let block = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let (b, s) = (block.clone(), started.clone());
    let refresh: RefreshFn = Arc::new(move |context| {
        if !context.allow_network && b.load(Ordering::SeqCst) {
            s.store(true, Ordering::SeqCst);
            return pending();
        }
        async { Ok(()) }.boxed()
    });
    let credentials = AuthStorage::in_memory(None);
    let runtime = runtime_with_provider(sync_provider("cancelled-sync", None, None, Some(refresh)), credentials.clone()).await;
    block.store(true, Ordering::SeqCst);
    let controller = CancellationToken::new();
    let login = {
        let (runtime, signal) = (runtime.clone(), controller.clone());
        tokio::spawn(async move {
            runtime.login("cancelled-sync", AuthType::ApiKey, interaction("unused"), Some(signal), None).await
        })
    };
    wait_for(&started).await;
    controller.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), login).await.unwrap().unwrap().unwrap_err();
    let error = sync_error(&error);
    assert_eq!(error.provider_id(), "cancelled-sync");
    assert_eq!(error.operation().as_str(), "login");
    assert_eq!(error.credential(), Some(api_key("cancelled-sync-key")));
    assert_eq!(credentials.read("cancelled-sync", None).await.unwrap(), Some(api_key("cancelled-sync-key")));
}

#[tokio::test]
async fn reports_committed_credentials_when_local_synchronization_fails() {
    let fail = Arc::new(AtomicBool::new(false));
    let f = fail.clone();
    let refresh: RefreshFn = Arc::new(move |context| {
        let failing = !context.allow_network && f.load(Ordering::SeqCst);
        async move {
            if failing {
                anyhow::bail!("cache restore failed");
            }
            Ok(())
        }
        .boxed()
    });
    let credentials = AuthStorage::in_memory(None);
    let runtime = runtime_with_provider(sync_provider("broken-sync", None, None, Some(refresh)), credentials.clone()).await;
    fail.store(true, Ordering::SeqCst);
    let error = runtime.login("broken-sync", AuthType::ApiKey, interaction("unused"), None, None).await.unwrap_err();
    let error = sync_error(&error);
    assert_eq!(error.provider_id(), "broken-sync");
    assert_eq!(error.operation().as_str(), "login");
    assert_eq!(error.credential(), Some(api_key("broken-sync-key")));
    assert_eq!(credentials.read("broken-sync", None).await.unwrap(), Some(api_key("broken-sync-key")));
}

// ---------------------------------------------------------------------------
// model-runtime-classifiers.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_jev_separately_and_classifies_with_runtime_resolved_auth() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let jev = runtime.get_model_of_type(ModelType::Classifier, "typesafe", "jev-latest").unwrap();
    let AnyModel::Classifier(jev) = jev else { panic!("expected classifier") };
    assert_eq!(jev.model_type, "classifier");
    assert!(runtime.get_model("typesafe", "jev-latest").is_none());

    let unconfigured = runtime.classify(jev.clone(), classifier_context(), None).await;
    assert_eq!(unconfigured.stop_reason, ClassifierStopReason::Error);
    assert!(unconfigured.error_message.unwrap_or_default().contains("not configured"));

    runtime.set_runtime_api_key("typesafe", "sk-typesafe", None).await.unwrap();
    let available = runtime.get_available_of_type(ModelType::Classifier, Some("typesafe"), None).await.unwrap();
    assert_eq!(available, vec![AnyModel::Classifier(jev.clone())]);

    // TS mocks `fetch`; here the request goes to a local server through the model's base URL.
    let server = mock_http::MockServer::start(|_, _| {
        mock_http::Response::json(200, json!({ "answers": { "approved": { "type": "noul", "noul": 0.8 } } }))
    })
    .await;
    let mut local = jev.clone();
    local.base_url = format!("{}/v1", server.url);
    let result = runtime.classify(local, classifier_context(), None).await;
    assert_eq!(server.request(0).header("authorization"), Some("Bearer sk-typesafe"));
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    assert_eq!(result.answers["approved"], ClassifierAnswer::Bool { probability: 0.8 });
}

/// A models.json routing `provider` through a local server standing in for TS's proxy URL.
async fn classifier_proxy_runtime(
    provider: &str,
    compat: Option<Value>,
) -> (Arc<ModelRuntime>, tempfile::TempDir, mock_http::MockServer) {
    let server = mock_http::MockServer::start(|_, _| {
        mock_http::Response::json(200, json!({ "answers": { "approved": { "type": "noul", "noul": 0.8 } } }))
    })
    .await;
    let mut config = json!({ "baseUrl": format!("{}/v1", server.url) });
    if let Some(compat) = compat {
        config["compat"] = compat;
    }
    let (runtime, dir) =
        runtime_with_models_json(AuthStorage::in_memory(None), json!({ "providers": { provider: config } })).await;
    (runtime, dir, server)
}

/// Classifies one image and returns the request the service received.
async fn capture_image_request(
    runtime: &ModelRuntime,
    server: &mock_http::MockServer,
    provider: &str,
    id: &str,
) -> mock_http::Request {
    runtime.set_runtime_api_key(provider, "secret", None).await.unwrap();
    let Some(AnyModel::Classifier(model)) = runtime.get_model_of_type(ModelType::Classifier, provider, id) else {
        panic!("missing classifier {provider}/{id}");
    };
    let mut context = classifier_context();
    context.images = Some(vec![ImageContent { data: "cG5n".into(), mime_type: "image/png".into() }]);
    let result = runtime.classify(model, context, None).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop, "{:?}", result.error_message);
    assert_eq!(server.requests().len(), 1);
    server.request(0)
}

#[tokio::test]
async fn keeps_openrouters_image_form_when_models_json_routes_openrouter_through_a_proxy() {
    let (runtime, _dir, server) = classifier_proxy_runtime("openrouter", None).await;

    let request = capture_image_request(&runtime, &server, "openrouter", "typesafe/jev-1.14").await;

    // OpenRouter answers TypeSafe `assets` with 200 but drops the images.
    assert_eq!(request.path, "/v1/systemone");
    let body = request.json();
    assert_eq!(
        body["state"],
        json!([{ "type": "image_url", "image_url": { "url": "data:image/png;base64,cG5n" } }, classifier_context().state])
    );
    assert!(body.get("assets").is_none());
    assert_eq!(request.header("typesafe-danger-zone-images"), None);
}

#[tokio::test]
async fn applies_provider_compat_from_models_json_to_classifier_models() {
    let (runtime, _dir, server) = classifier_proxy_runtime(
        "typesafe",
        Some(json!({ "imageInput": "state-image-urls", "sendImageOptInHeader": false })),
    )
    .await;

    let request = capture_image_request(&runtime, &server, "typesafe", "jev-latest").await;

    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(
        request.json()["state"],
        json!([{ "type": "image_url", "image_url": { "url": "data:image/png;base64,cG5n" } }, classifier_context().state])
    );
    assert_eq!(request.header("typesafe-danger-zone-images"), None);
}

// ---------------------------------------------------------------------------
// model-runtime-images.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_built_in_openrouter_image_models_separately_from_chat_models() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let images = runtime.get_models_of_type(ModelType::Image, Some("openrouter"));
    assert!(!images.is_empty());
    assert!(runtime.get_models(Some("openrouter")).iter().all(|m| m.model_type.as_deref().is_none_or(|t| t == "chat")));
    assert_eq!(runtime.get_model_of_type(ModelType::Image, "openrouter", images[0].id()), Some(images[0].clone()));
    assert_eq!(
        runtime.get_model("openrouter", "google/gemini-3-pro-image").map(|m| m.api),
        Some("openai-completions".to_string())
    );
    assert!(matches!(
        runtime.get_model_of_type(ModelType::Image, "openrouter", "google/gemini-3-pro-image"),
        Some(AnyModel::Image(_))
    ));
    let classifiers = runtime.get_models_of_type(ModelType::Classifier, Some("openrouter"));
    assert_eq!(
        runtime.get_all_models(Some("openrouter")).len(),
        runtime.get_models(Some("openrouter")).len() + images.len() + classifiers.len()
    );
}

struct OkImages;

#[async_trait]
impl ProviderImages for OkImages {
    async fn generate_images(&self, model: &ImageModel, _context: &ImagesContext, _options: StreamOptions) -> AssistantImages {
        ok_image_result(model)
    }
}

type Observed = Arc<Mutex<Vec<(Option<String>, Option<ProviderHeaders>)>>>;

struct ObservingImages(Observed);

#[async_trait]
impl ProviderImages for ObservingImages {
    async fn generate_images(&self, model: &ImageModel, _context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        self.0.lock().push((options.api_key.clone(), options.headers.clone()));
        ok_image_result(model)
    }
}

struct ObservingClassifier(Observed);

#[async_trait]
impl ProviderClassifier for ObservingClassifier {
    async fn classify(&self, model: &ClassifierModel, _context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        self.0.lock().push((options.api_key.clone(), options.headers.clone()));
        serde_json::from_value(json!({
            "api": model.api, "provider": model.provider, "model": model.id,
            "answers": { "approved": { "type": "bool", "probability": 0.9 } },
            "stopReason": "stop", "timestamp": 0
        }))
        .unwrap()
    }
}

fn resolve_always() -> ResolveFn {
    Arc::new(|_| async { Ok(Some(AuthResult { auth: ModelAuth::default(), env: None, source: None })) }.boxed())
}

#[tokio::test]
async fn extension_model_lists_replace_undeclared_models_of_every_operation() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let mut chat = model_json("mixed", "built-in-chat", "https://built-in.test/v1");
    chat.api = "test-chat".into();
    chat.name = "Built-in chat".into();
    runtime
        .register_native_provider(
            create_provider(CreateProviderOptions {
                id: "mixed".into(),
                auth: ProviderAuth {
                    api_key: Some(Arc::new(TestApiKeyAuth { name: "Mixed key".into(), login: None, check: None, resolve: resolve_always() })),
                    oauth: None,
                },
                models: vec![
                    AnyModel::Chat(chat.clone()),
                    AnyModel::Image(image_model("mixed", "built-in-image")),
                    AnyModel::Classifier(classifier_model("mixed", "built-in-classifier")),
                ],
                images: [("test-images".to_string(), Arc::new(OkImages) as Arc<dyn ProviderImages>)].into_iter().collect(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

    let mut extension_chat = chat_config("extension-chat");
    if let ProviderModelConfig::Chat(c) = &mut extension_chat {
        c.name = "Extension chat".into();
        c.api = Some("test-chat".into());
        c.base_url = Some("https://chat-proxy.test/v1".into());
        c.context_window = 1000;
        c.max_tokens = 100;
    }
    runtime
        .register_provider(
            "mixed",
            &ProviderConfig { api_key: Some("extension-secret".into()), models: Some(vec![extension_chat]), ..Default::default() },
        )
        .unwrap();

    let all: Vec<(String, String)> = runtime
        .get_all_models(Some("mixed"))
        .iter()
        .map(|m| (m.model_type().as_str().to_string(), m.id().to_string()))
        .collect();
    assert_eq!(all, vec![("chat".to_string(), "extension-chat".to_string())]);
    assert_eq!(
        runtime.get_model("mixed", "extension-chat").map(|m| m.base_url),
        Some("https://chat-proxy.test/v1".to_string())
    );
    assert!(runtime.get_models_of_type(ModelType::Image, Some("mixed")).is_empty());
    assert!(runtime.get_models_of_type(ModelType::Classifier, Some("mixed")).is_empty());
}

#[tokio::test]
async fn registers_extension_image_and_classifier_models_with_their_implementations() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let mut image: Value = serde_json::to_value(image_model("ignored", "shared")).unwrap();
    image["headers"] = json!({ "X-Operation": "image" });
    let mut classifier: Value = serde_json::to_value(classifier_model("ignored", "shared")).unwrap();
    classifier["headers"] = json!({ "X-Operation": "classifier" });
    runtime
        .register_provider(
            "extension-operations",
            &ProviderConfig {
                api_key: Some("extension-secret".into()),
                models: Some(vec![serde_json::from_value(image).unwrap(), serde_json::from_value(classifier).unwrap()]),
                images: Some(
                    [("test-images".to_string(), Arc::new(ObservingImages(observed.clone())) as Arc<dyn ProviderImages>)]
                        .into_iter()
                        .collect(),
                ),
                classifiers: Some(
                    [(
                        "test-classifier".to_string(),
                        Arc::new(ObservingClassifier(observed.clone())) as Arc<dyn ProviderClassifier>,
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
    let Some(AnyModel::Image(image)) = runtime.get_model_of_type(ModelType::Image, "extension-operations", "shared") else {
        panic!("image model")
    };
    let Some(AnyModel::Classifier(classifier)) =
        runtime.get_model_of_type(ModelType::Classifier, "extension-operations", "shared")
    else {
        panic!("classifier model")
    };
    assert_eq!(runtime.generate_images(image, images_context(), None).await.stop_reason, ImagesStopReason::Stop);
    assert_eq!(runtime.classify(classifier, classifier_context(), None).await.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(
        observed.lock().clone(),
        vec![
            (Some("extension-secret".to_string()), Some(headers(&[("X-Operation", "image")]))),
            (Some("extension-secret".to_string()), Some(headers(&[("X-Operation", "classifier")]))),
        ]
    );
}

#[tokio::test]
async fn generates_images_through_a_native_provider_with_runtime_resolved_auth() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let calls: Observed = Arc::new(Mutex::new(Vec::new()));
    runtime
        .register_native_provider(
            create_provider(CreateProviderOptions {
                id: "pixels".into(),
                auth: ProviderAuth {
                    api_key: Some(Arc::new(TestApiKeyAuth {
                        name: "Pixels key".into(),
                        login: None,
                        check: None,
                        resolve: stored_key_resolve(),
                    })),
                    oauth: None,
                },
                models: vec![AnyModel::Image(image_model("pixels", "flux"))],
                images: [("test-images".to_string(), Arc::new(ObservingImages(calls.clone())) as Arc<dyn ProviderImages>)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    let Some(AnyModel::Image(model)) = runtime.get_model_of_type(ModelType::Image, "pixels", "flux") else { panic!() };

    let unconfigured = runtime.generate_images(model.clone(), images_context(), None).await;
    assert_eq!(unconfigured.stop_reason, ImagesStopReason::Error);
    assert!(unconfigured.error_message.unwrap_or_default().contains("not configured"));
    assert!(calls.lock().is_empty());

    let controller = CancellationToken::new();
    controller.cancel();
    let cancelled = runtime
        .generate_images(model.clone(), images_context(), Some(StreamOptions { signal: Some(controller), ..Default::default() }.into()))
        .await;
    assert_eq!(cancelled.stop_reason, ImagesStopReason::Aborted);
    assert!(calls.lock().is_empty());

    runtime.set_runtime_api_key("pixels", "sk-pixels", None).await.unwrap();
    let available: Vec<String> = runtime
        .get_available_of_type(ModelType::Image, Some("pixels"), None)
        .await
        .unwrap()
        .iter()
        .map(|m| m.id().to_string())
        .collect();
    assert_eq!(available, vec!["flux".to_string()]);
    let result = runtime.generate_images(model, images_context(), None).await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(calls.lock()[0].0.as_deref(), Some("sk-pixels"));
}

struct RejectingStreams(Arc<AtomicUsize>);

impl ProviderStreams for RejectingStreams {
    fn stream(&self, _model: &Model, _context: &TranscriptContext, _options: StreamOptions) -> AssistantMessageEventStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("image reached chat adapter")
    }
    fn stream_simple(&self, _model: &Model, _context: &TranscriptContext, _options: StreamOptions) -> AssistantMessageEventStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("image reached chat adapter")
    }
    fn supports_fetch_deferred(&self) -> bool {
        true
    }
    fn fetch_deferred(&self, _model: &Model, _handle: &DeferredHandle, _options: StreamOptions) -> AssistantMessageEventStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("image reached chat adapter")
    }
    fn supports_cancel_deferred(&self) -> bool {
        true
    }
    fn cancel_deferred(&self, _model: &Model, _handle: &DeferredHandle, _options: StreamOptions) -> BoxFuture<'static, anyhow::Result<()>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        async { anyhow::bail!("image reached chat adapter") }.boxed()
    }
}

#[tokio::test]
async fn rejects_image_models_at_every_chat_entry_point_before_provider_dispatch() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let dispatches = Arc::new(AtomicUsize::new(0));
    runtime
        .register_native_provider(
            create_provider(CreateProviderOptions {
                id: "hybrid".into(),
                auth: ProviderAuth {
                    api_key: Some(Arc::new(TestApiKeyAuth {
                        name: "Hybrid key".into(),
                        login: None,
                        check: None,
                        resolve: stored_key_resolve(),
                    })),
                    oauth: None,
                },
                models: vec![AnyModel::Image(image_model("hybrid", "shared"))],
                api: Some(ProviderApi::Single(Arc::new(RejectingStreams(dispatches.clone())))),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    runtime.set_runtime_api_key("hybrid", "sk-hybrid", None).await.unwrap();
    let Some(AnyModel::Image(image)) = runtime.get_model_of_type(ModelType::Image, "hybrid", "shared") else { panic!() };
    // TS casts the image model to a chat model; the Rust equivalent is the same JSON read as `Model`.
    let mut chat_json = serde_json::to_value(&image).unwrap();
    chat_json["reasoning"] = json!(false);
    chat_json["contextWindow"] = json!(0);
    chat_json["maxTokens"] = json!(0);
    let chat: Model = serde_json::from_value(chat_json).unwrap();
    let handle = DeferredHandle {
        provider: "hybrid".into(),
        model_id: image.id.clone(),
        api: image.api.clone(),
        id: "response-1".into(),
        expires_at: None,
        ..serde_json::from_value(json!({ "provider": "", "modelId": "", "api": "", "id": "" })).unwrap()
    };
    let results = vec![
        runtime.stream(chat.clone(), Context::default(), None).result().await,
        runtime.complete(chat.clone(), Context::default(), None).await,
        runtime.stream_simple(chat.clone(), Context::default(), None).result().await,
        runtime.complete_simple(chat.clone(), Context::default(), None).await,
        runtime.stream_deferred(chat.clone(), handle.clone(), None).result().await,
        runtime.fetch_deferred(chat.clone(), handle.clone(), None).await,
    ];
    for result in results {
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error_message.unwrap_or_default().contains("is not a chat model"));
    }
    let error = runtime.cancel_deferred(chat, handle, None).await.unwrap_err();
    assert!(error.to_string().contains("is not a chat model"));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn keeps_image_generation_on_a_built_in_provider_composed_with_models_json_overrides() {
    let (runtime, _dir) = runtime_with_models_json(
        AuthStorage::in_memory(None),
        json!({
            "providers": {
                "openrouter": {
                    "headers": { "X-Title": "pi" },
                    "modelOverrides": {
                        "openrouter/auto": { "name": "Auto (renamed)" },
                        "google/gemini-3-pro-image": { "headers": { "X-Chat-Only": "yes" } }
                    }
                }
            }
        }),
    )
    .await;
    let provider = runtime.get_provider("openrouter").unwrap();
    assert!(provider.supports_images());
    assert_eq!(runtime.get_model("openrouter", "openrouter/auto").map(|m| m.name), Some("Auto (renamed)".to_string()));
    assert!(!runtime.get_models_of_type(ModelType::Image, Some("openrouter")).is_empty());

    // Auth is provider-scoped: the same key serves chat and image models.
    runtime.set_runtime_api_key("openrouter", "sk-or", None).await.unwrap();
    let chat = runtime.get_model("openrouter", "google/gemini-3-pro-image").unwrap();
    let image = runtime.get_model_of_type(ModelType::Image, "openrouter", "google/gemini-3-pro-image").unwrap();
    let chat_auth = runtime.get_auth(&(&chat).into(), None).await.unwrap().unwrap();
    let image_auth = runtime.get_auth(&ModelRuntimeGetAuthProviderOrModel::AnyModel(image), None).await.unwrap().unwrap();
    assert_eq!(image_auth.auth.api_key.as_deref(), Some("sk-or"));
    let chat_headers = chat_auth.auth.headers.unwrap();
    assert_eq!(chat_headers.get("X-Title"), Some(&Some("pi".to_string())));
    assert_eq!(chat_headers.get("X-Chat-Only"), Some(&Some("yes".to_string())));
    assert_eq!(image_auth.auth.headers, Some(headers(&[("X-Title", "pi")])));
}

#[tokio::test]
async fn does_not_add_image_generation_or_classification_to_composed_chat_only_providers() {
    let (runtime, _dir) = runtime_with_models_json(
        AuthStorage::in_memory(None),
        json!({ "providers": { "anthropic": { "headers": { "X-Title": "pi" } } } }),
    )
    .await;
    let provider = runtime.get_provider("anthropic").unwrap();
    assert!(!provider.supports_images());
    assert!(!provider.supports_classify());
    assert!(runtime.get_provider("openrouter").unwrap().supports_images());
    assert!(runtime.get_provider("typesafe").unwrap().supports_classify());
}

// ---------------------------------------------------------------------------
// model-runtime-modify-models-compat.test.ts
// ---------------------------------------------------------------------------

fn native_provider(id: &str, model: Model, api_key_auth: Arc<dyn ApiKeyAuth>) -> TestProvider {
    TestProvider {
        id: id.into(),
        name: "Extension Native".into(),
        auth: ProviderAuth { api_key: Some(api_key_auth), oauth: None },
        models: vec![model],
        refresh: None,
        deferred: None,
    }
}

/// Native-provider auth of the lifecycle tests: prompt login, stored check/resolve.
struct NativeSetupAuth;

#[async_trait]
impl ApiKeyAuth for NativeSetupAuth {
    fn name(&self) -> &str {
        "Native setup"
    }
    fn supports_login(&self) -> bool {
        true
    }
    async fn login(&self, interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        let key = interaction.interaction.prompt(AuthPrompt::secret("API key")).await?;
        Ok(ApiKeyCredential { key: Some(key), env: None })
    }
    fn supports_check(&self) -> bool {
        true
    }
    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        Ok(input
            .credential
            .and_then(|c| c.key)
            .map(|_| AuthCheck { source: Some("stored native key".into()), auth_type: AuthType::ApiKey }))
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        Ok(input.credential.and_then(|c| c.key).map(|key| AuthResult {
            auth: ModelAuth { api_key: Some(key), base_url: Some("https://resolved.test/v1".into()), headers: None },
            env: None,
            source: Some("stored native key".into()),
        }))
    }
}

fn fixed_key_auth() -> Arc<dyn ApiKeyAuth> {
    Arc::new(TestApiKeyAuth {
        name: "Native key".into(),
        login: None,
        check: None,
        resolve: Arc::new(|_| {
            async {
                Ok(Some(AuthResult {
                    auth: ModelAuth { api_key: Some("key".into()), ..Default::default() },
                    env: None,
                    source: Some("native".into()),
                }))
            }
            .boxed()
        }),
    })
}

#[tokio::test]
async fn registers_native_pi_ai_providers_with_their_auth_implementation() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let provider: Arc<dyn Provider> = Arc::new(native_provider(
        "extension-native",
        model_json("extension-native", "native", "https://fallback.test/v1"),
        Arc::new(NativeSetupAuth),
    ));
    runtime.register_native_provider(provider.clone()).unwrap();
    let registry = ModelRegistry::new(runtime.clone());
    assert!(Arc::ptr_eq(&registry.get_provider("extension-native").unwrap(), &provider));
    assert!(Arc::ptr_eq(&registry.get_registered_native_provider("extension-native").unwrap(), &provider));
    assert!(registry.get_registered_provider_ids().contains(&"extension-native".to_string()));
    assert!(registry.find("extension-native", "native").is_some());

    runtime.login("extension-native", AuthType::ApiKey, interaction("secret"), None, None).await.unwrap();
    let auth = registry.get_provider_auth("extension-native").await.unwrap().unwrap();
    assert_eq!(auth.auth.api_key.as_deref(), Some("secret"));
    assert_eq!(auth.auth.base_url.as_deref(), Some("https://resolved.test/v1"));

    registry.unregister_provider("extension-native");
    assert!(registry.get_provider("extension-native").is_none());
}

struct NativeOAuth;

#[async_trait]
impl OAuthAuth for NativeOAuth {
    fn name(&self) -> &str {
        "Native OAuth"
    }
    async fn login(&self, _interaction: ProviderAuthInteraction, _options: Option<LoginOptions>) -> anyhow::Result<OAuthCredential> {
        panic!("unused")
    }
    async fn refresh(&self, credential: OAuthCredential, _signal: CancellationToken) -> anyhow::Result<OAuthCredential> {
        Ok(credential)
    }
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        Ok(ModelAuth { api_key: Some(credential.access.clone()), ..Default::default() })
    }
}

// Regression for #9962: initial model selection reads the snapshot before the async refresh finishes.
#[tokio::test]
async fn marks_a_native_provider_with_a_stored_credential_as_configured_when_it_registers() {
    let runtime = runtime_with(auth_storage(&[("extension-native", oauth("access", "refresh", now_ms() + 3_600_000))])).await;
    let provider = TestProvider {
        id: "extension-native".into(),
        name: "Extension Native".into(),
        auth: ProviderAuth { api_key: None, oauth: Some(Arc::new(NativeOAuth)) },
        models: vec![model_json("extension-native", "native", "https://example.test/v1")],
        refresh: None,
        deferred: None,
    };
    runtime.register_native_provider(Arc::new(provider)).unwrap();
    assert!(runtime.has_configured_auth("extension-native"));
    assert!(runtime.is_using_o_auth("extension-native"));
    assert!(
        runtime
            .get_available_snapshot()
            .iter()
            .any(|m| format!("{}/{}", m.provider, m.id) == "extension-native/native")
    );
    runtime.refresh(refresh_opts(false, &[])).await.unwrap();
    assert!(runtime.has_configured_auth("extension-native"));
}

#[tokio::test]
async fn preserves_native_deferred_methods_through_provider_overlays() {
    let (runtime, _dir) = runtime_with_models_json(
        AuthStorage::in_memory(None),
        json!({ "providers": { "extension-native-deferred": { "baseUrl": "https://overlay.test/v1" } } }),
    )
    .await;
    let capture = Arc::new(Mutex::new(DeferredCapture::default()));
    let mut provider = native_provider(
        "extension-native-deferred",
        model_json("extension-native-deferred", "native-deferred", "https://native.test/v1"),
        fixed_key_auth(),
    );
    provider.deferred = Some(capture.clone());
    runtime.register_native_provider(Arc::new(provider)).unwrap();
    let composed = runtime.get_model("extension-native-deferred", "native-deferred").unwrap();
    let handle = |id: &str| -> DeferredHandle {
        serde_json::from_value(json!({
            "provider": "extension-native-deferred", "modelId": "native-deferred", "api": "openai-completions", "id": id
        }))
        .unwrap()
    };
    let mut fetch_options = StreamOptions { headers: Some(headers(&[("X-Fetch", "fetch")])), ..Default::default() };
    fetch_options.extra.insert("wait".into(), json!(25));
    runtime
        .fetch_deferred(
            composed.clone(),
            handle("fetch-id"),
            Some(ModelsRequestOptions {
                options: fetch_options,
                transform_headers: Some(Arc::new(|mut h: ProviderHeaders| {
                    h.insert("X-Transformed".into(), Some("fetch".into()));
                    async move { h }.boxed()
                })),
            }),
        )
        .await;
    runtime
        .cancel_deferred(
            composed,
            handle("cancel-id"),
            Some(ModelsRequestOptions {
                options: StreamOptions { timeout_ms: Some(100), ..Default::default() },
                transform_headers: Some(Arc::new(|mut h: ProviderHeaders| {
                    h.insert("X-Transformed".into(), Some("cancel".into()));
                    async move { h }.boxed()
                })),
            }),
        )
        .await
        .unwrap();

    let capture = capture.lock();
    assert_eq!(capture.fetched_base_url.as_deref(), Some("https://overlay.test/v1"));
    let fetched = capture.fetched_options.as_ref().unwrap();
    assert_eq!(fetched.api_key.as_deref(), Some("key"));
    assert_eq!(fetched.extra.get("wait"), Some(&json!(25)));
    let fetched_headers = fetched.headers.clone().unwrap();
    assert_eq!(fetched_headers.get("X-Fetch"), Some(&Some("fetch".to_string())));
    assert_eq!(fetched_headers.get("X-Transformed"), Some(&Some("fetch".to_string())));
    assert_eq!(capture.cancelled_id.as_deref(), Some("cancel-id"));
    let cancelled = capture.cancelled_options.as_ref().unwrap();
    assert_eq!(cancelled.api_key.as_deref(), Some("key"));
    assert_eq!(cancelled.timeout_ms, Some(100));
    assert_eq!(cancelled.headers.clone().unwrap().get("X-Transformed"), Some(&Some("cancel".to_string())));
}

#[tokio::test]
async fn applies_models_json_overrides_above_native_providers() {
    let (runtime, _dir) = runtime_with_models_json(
        AuthStorage::in_memory(None),
        json!({ "providers": { "extension-native": { "modelOverrides": { "native": { "contextWindow": 4242 } } } } }),
    )
    .await;
    runtime
        .register_native_provider(Arc::new(native_provider(
            "extension-native",
            model_json("extension-native", "native", "https://native.test/v1"),
            fixed_key_auth(),
        )))
        .unwrap();
    assert_eq!(runtime.get_model("extension-native", "native").map(|m| m.context_window), Some(4242));
}

#[tokio::test]
async fn publishes_refresh_models_results_without_forcing_models_store_persistence() {
    let models_store = Arc::new(InMemoryModelsStore::new());
    let runtime = ModelRuntime::create(Some(&CreateModelRuntimeOptions {
        credentials: Some(AuthStorage::in_memory(None)),
        models_path: Some(String::new()),
        models_store: Some(models_store.clone()),
        allow_model_network: Some(false),
        ..Default::default()
    }))
    .await
    .unwrap();
    let mut live = chat_config("live");
    if let ProviderModelConfig::Chat(c) = &mut live {
        c.base_url = Some("http://localhost:8080/v1".into());
        c.api = Some("openai-completions".into());
    }
    runtime
        .register_provider(
            "extension-dynamic",
            &ProviderConfig {
                base_url: Some("http://localhost:8080/v1".into()),
                api_key: Some("local".into()),
                api: Some("openai-completions".into()),
                refresh_models: Some(Arc::new(move |_| {
                    let live = live.clone();
                    async move { Ok(vec![live]) }.boxed()
                })),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.refresh(refresh_opts(false, &[])).await.unwrap();
    assert!(runtime.get_model("extension-dynamic", "live").is_some());
    assert!(models_store.read("extension-dynamic", None).await.unwrap().is_none());
}

#[tokio::test]
async fn applies_legacy_oauth_modify_models_after_async_credential_initialization() {
    let runtime = runtime_with(auth_storage(&[("extension-oauth", oauth("access", "refresh", now_ms() + 60_000))])).await;
    let mut config = extension_oauth_config(None, None);
    config.name = "Extension OAuth".into();
    config.login = Arc::new(|_| async { anyhow::bail!("not used") }.boxed());
    config.modify_models = Some(Arc::new(|mut models: Vec<Model>, credential: &OAuthCredentials| {
        if credential.access == "access" {
            models.push(model_json("extension-oauth", "credential-model", "https://example.test/v1"));
        }
        models
    }));
    runtime
        .register_provider(
            "extension-oauth",
            &ProviderConfig {
                base_url: Some("https://example.test/v1".into()),
                api: Some("openai-completions".into()),
                models: Some(vec![chat_config("base")]),
                oauth: Some(config),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.refresh(refresh_opts(false, &[])).await.unwrap();
    assert!(runtime.get_model("extension-oauth", "base").is_some());
    assert!(runtime.get_model("extension-oauth", "credential-model").is_some());

    runtime.logout("extension-oauth", None).await.unwrap();
    assert!(runtime.get_model("extension-oauth", "credential-model").is_none());
}

// ---------------------------------------------------------------------------
// model-runtime-cloudflare-compat.test.ts
// ---------------------------------------------------------------------------

const CLOUDFLARE_COMPAT_BASE: &str = "https://gateway.ai.cloudflare.com/v1/test-account/test-gateway/compat";

async fn cloudflare_runtime() -> Arc<ModelRuntime> {
    let storage = AuthStorage::in_memory(None);
    storage
        .modify(
            "cloudflare-ai-gateway",
            Box::new(|_| {
                async {
                    Ok(Some(Credential::ApiKey(ApiKeyCredential {
                        key: Some("test-token".into()),
                        env: Some(
                            [
                                ("CLOUDFLARE_ACCOUNT_ID".to_string(), "test-account".to_string()),
                                ("CLOUDFLARE_GATEWAY_ID".to_string(), "test-gateway".to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    })))
                }
                .boxed()
            }),
            None,
        )
        .await
        .unwrap();
    runtime_with(storage).await
}

fn sse_stop_response() -> mock_http::Response {
    let chunk = json!({
        "id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 0, "model": "test",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    });
    mock_http::Response::sse(&format!("data: {chunk}\n\ndata: [DONE]\n\n"))
}

/// The catalog model with its `https://gateway.ai.cloudflare.com` origin swapped for the local
/// server (TS captures the request with a `fetch` mock instead). The account/gateway placeholders
/// stay, so the request path shows how the endpoint was materialized.
fn local_cloudflare_model(model: &Model, server: &mock_http::MockServer) -> Model {
    assert!(model.base_url.starts_with("https://gateway.ai.cloudflare.com/"), "{}", model.base_url);
    Model { base_url: model.base_url.replace("https://gateway.ai.cloudflare.com", &server.url), ..model.clone() }
}

const CLOUDFLARE_COMPAT_PATH: &str = "/v1/test-account/test-gateway/compat/chat/completions";

#[tokio::test]
async fn materializes_the_cloudflare_endpoint_through_model_runtime_streaming() {
    let runtime = cloudflare_runtime().await;
    let model = runtime.get_model("cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6").unwrap();
    let auth = runtime.get_auth(&(&model).into(), None).await.unwrap().unwrap();
    assert_eq!(
        pi_ai::providers::cloudflare::resolve_cloudflare_base_url(&model.base_url, auth.env.as_ref()),
        CLOUDFLARE_COMPAT_BASE
    );

    let server = mock_http::MockServer::start(|_, _| sse_stop_response()).await;
    let result = runtime.complete_simple(local_cloudflare_model(&model, &server), Context::default(), None).await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, CLOUDFLARE_COMPAT_PATH);
    assert_eq!(requests[0].header("cf-aig-authorization"), Some("Bearer test-token"));
}

#[tokio::test]
async fn materializes_the_cloudflare_endpoint_after_extension_style_auth_resolution() {
    let runtime = cloudflare_runtime().await;
    let registry = ModelRegistry::new(runtime);
    let model = registry.find("cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6").unwrap();
    let ResolvedRequestAuth::Ok(auth) = registry.get_api_key_and_headers(&model).await else {
        panic!("auth should resolve")
    };
    let headers = auth.headers.clone().unwrap();
    assert_eq!(headers.get("cf-aig-authorization"), Some(&Some("Bearer test-token".to_string())));
    assert_eq!(headers.get("Authorization"), Some(&None));
    assert_eq!(headers.get("x-api-key"), Some(&None));

    // TS passes `{ ...auth, fetch }` to the compat `complete()`.
    let server = mock_http::MockServer::start(|_, _| sse_stop_response()).await;
    let options = StreamOptions {
        api_key: auth.api_key.clone(),
        headers: auth.headers.clone(),
        env: auth.env.as_ref().map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        ..Default::default()
    };
    let result = pi_ai::compat::complete(&local_cloudflare_model(&model, &server), &Context::default(), options).await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, CLOUDFLARE_COMPAT_PATH);
    assert_eq!(requests[0].header("cf-aig-authorization"), Some("Bearer test-token"));
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(requests[0].header("x-api-key"), None);
}


// ---------------------------------------------------------------------------
// suite/regressions/7301-stalled-availability-refresh.test.ts. TS replaces `authStorage.list` and
// `models.getAvailable`; here a credential store wrapper stalls `list`, and a native provider's
// auth check stalls the provider-scoped pass.
// ---------------------------------------------------------------------------

use pi_coding_agent::core::provider_composer::{AuthStatus, AuthStatusSource};

/// Credential store whose next `list` waits for `release` (or fails with `fail`).
struct StallingStore {
    inner: Arc<AuthStorage>,
    stall: AtomicBool,
    started: tokio::sync::Notify,
    started_flag: AtomicBool,
    gate: tokio::sync::Notify,
    failure: Mutex<Option<String>>,
    calls: AtomicUsize,
}

impl StallingStore {
    fn new(inner: Arc<AuthStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            stall: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            started_flag: AtomicBool::new(false),
            gate: tokio::sync::Notify::new(),
            failure: Mutex::new(None),
            calls: AtomicUsize::new(0),
        })
    }
    fn stall_next_list(&self) {
        self.stall.store(true, Ordering::SeqCst);
    }
    async fn wait_started(&self) {
        while !self.started_flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    fn release(&self) {
        self.gate.notify_one();
    }
    fn fail(&self, message: &str) {
        *self.failure.lock() = Some(message.to_string());
        self.gate.notify_one();
    }
}

#[async_trait]
impl CredentialStore for StallingStore {
    async fn read(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        self.inner.read(provider_id, signal).await
    }
    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        let entries = self.inner.list(signal).await?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.stall.swap(false, Ordering::SeqCst) {
            return Ok(entries);
        }
        self.started_flag.store(true, Ordering::SeqCst);
        self.started.notify_waiters();
        self.gate.notified().await;
        if let Some(message) = self.failure.lock().take() {
            anyhow::bail!(message);
        }
        Ok(entries)
    }
    async fn modify(&self, provider_id: &str, f: ModifyCredentialFn, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        self.inner.modify(provider_id, f, signal).await
    }
    async fn delete(&self, provider_id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        self.inner.delete(provider_id, signal).await
    }
}

async fn wait_for_recovery_refresh(store: &StallingStore, runtime: &Arc<ModelRuntime>) {
    let refresh = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.refresh(refresh_opts(false, &[])).await })
    };
    for _ in 0..1000 {
        if store.calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(store.calls.load(Ordering::SeqCst), 2);
    refresh.await.unwrap().unwrap();
}

fn stored_status() -> AuthStatus {
    AuthStatus::configured(AuthStatusSource::Stored, None)
}

#[tokio::test]
async fn recovers_without_letting_the_stalled_refresh_overwrite_the_newer_snapshot() {
    let store = StallingStore::new(AuthStorage::in_memory(None));
    let runtime = runtime_with(store.clone()).await;
    store.modify("stale-provider", Box::new(|_| async { Ok(Some(api_key("stale-key"))) }.boxed()), None).await.unwrap();
    runtime.refresh(refresh_opts(false, &[])).await.unwrap();
    assert_eq!(runtime.get_provider_auth_status("stale-provider"), stored_status());

    store.stall_next_list();
    store.calls.store(0, Ordering::SeqCst);
    let stale_refresh = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.get_available(None, None).await })
    };
    store.wait_started().await;

    store.delete("stale-provider", None).await.unwrap();
    store.modify("current-provider", Box::new(|_| async { Ok(Some(api_key("current-key"))) }.boxed()), None).await.unwrap();
    wait_for_recovery_refresh(&store, &runtime).await;
    assert_eq!(runtime.get_provider_auth_status("stale-provider"), AuthStatus::unconfigured());
    assert_eq!(runtime.get_provider_auth_status("current-provider"), stored_status());

    store.release();
    stale_refresh.await.unwrap().unwrap();
    assert_eq!(runtime.get_provider_auth_status("stale-provider"), AuthStatus::unconfigured());
    assert_eq!(runtime.get_provider_auth_status("current-provider"), stored_status());
}

#[tokio::test]
async fn does_not_let_a_stale_failure_overwrite_newer_availability_error_state() {
    let store = StallingStore::new(AuthStorage::in_memory(None));
    let runtime = runtime_with(store.clone()).await;
    store.stall_next_list();
    store.calls.store(0, Ordering::SeqCst);
    let stale_refresh = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.get_available(None, None).await })
    };
    store.wait_started().await;
    wait_for_recovery_refresh(&store, &runtime).await;
    assert_eq!(runtime.get_error(), None);

    store.fail("stale credential list failure");
    let error = stale_refresh.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("stale credential list failure"), "{error}");
    assert_eq!(runtime.get_error(), None);
}

#[tokio::test]
async fn does_not_let_a_stale_provider_scoped_failure_overwrite_a_newer_availability_pass() {
    let runtime = runtime_with(AuthStorage::in_memory(None)).await;
    let stall = Arc::new(AtomicBool::new(true));
    let started = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(tokio::sync::Notify::new());
    let (s, st, g) = (stall.clone(), started.clone(), gate.clone());
    // The first check (the provider-scoped pass) stalls, then fails; later checks succeed.
    let check: CheckFn = Arc::new(move |_| {
        if s.swap(false, Ordering::SeqCst) {
            st.store(true, Ordering::SeqCst);
            let g = g.clone();
            return async move {
                g.notified().await;
                anyhow::bail!("stale provider availability failure")
            }
            .boxed();
        }
        async { Ok(Some(AuthCheck { source: Some("stored".into()), auth_type: AuthType::ApiKey })) }.boxed()
    });
    runtime.register_native_provider(sync_provider("scoped", None, Some(check), None)).unwrap();
    // Let the registration's background refresh run without the stall.
    stall.store(false, Ordering::SeqCst);
    runtime.refresh(refresh_opts(false, &["scoped"])).await.unwrap();
    stall.store(true, Ordering::SeqCst);

    let stale_refresh = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.get_available(Some("scoped"), None).await })
    };
    wait_for(&started).await;
    runtime.get_available(None, None).await.unwrap();
    assert_eq!(runtime.get_error(), None);

    gate.notify_one();
    let error = stale_refresh.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("stale provider availability failure"), "{error:#}");
    assert_eq!(runtime.get_error(), None);
}

// ---------------------------------------------------------------------------
// suite/regressions/7027-credential-refresh-hang.test.ts (the ModelRuntime case; the
// `completeProviderAuthentication` cases belong to interactive mode).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn does_not_hold_login_behind_an_older_stalled_network_catalog_refresh() {
    let network_started = Arc::new(AtomicBool::new(false));
    let started = network_started.clone();
    let refresh: RefreshFn = Arc::new(move |context| {
        if !context.allow_network {
            return async { Ok(()) }.boxed();
        }
        started.store(true, Ordering::SeqCst);
        pending()
    });
    let login: LoginFn = Arc::new(|| async { Ok(ApiKeyCredential { key: Some("secret".into()), env: None }) }.boxed());
    // Unlike `sync_provider`, resolve falls back to an ambient key, so the network phase runs.
    let resolve: ResolveFn = Arc::new(|credential: Option<ApiKeyCredential>| {
        async move {
            let key = credential.and_then(|c| c.key);
            let source = if key.is_some() { "stored key" } else { "ambient key" };
            Ok(Some(AuthResult {
                auth: ModelAuth { api_key: Some(key.unwrap_or_else(|| "ambient-key".into())), ..Default::default() },
                env: None,
                source: Some(source.into()),
            }))
        }
        .boxed()
    });
    let provider = Arc::new(TestProvider {
        id: "stalled-login".into(),
        name: "Stalled Login".into(),
        auth: ProviderAuth {
            api_key: Some(Arc::new(TestApiKeyAuth {
                name: "API key".into(),
                login: Some(login),
                check: Some(Arc::new(|credential: Option<ApiKeyCredential>| {
                    async move {
                        Ok(credential
                            .and_then(|c| c.key)
                            .map(|_| AuthCheck { source: Some("stored key".into()), auth_type: AuthType::ApiKey }))
                    }
                    .boxed()
                })),
                resolve,
            })),
            oauth: None,
        },
        models: vec![model_json("stalled-login", "dynamic", "https://example.test/v1")],
        refresh: Some(refresh),
        deferred: None,
    });
    let credentials = AuthStorage::in_memory(None);
    let runtime = runtime_with_provider(provider, credentials.clone()).await;

    let stalled_refresh = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.refresh(refresh_opts(true, &["stalled-login"])).await })
    };
    wait_for(&network_started).await;
    let credential = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.login("stalled-login", AuthType::ApiKey, interaction("unused"), None, None),
    )
    .await
    .expect("login is not held behind the stalled refresh")
    .unwrap();
    assert_eq!(credential, api_key("secret"));
    assert!(runtime.get_available_snapshot().iter().any(|m| m.id == "dynamic"));
    assert_eq!(credentials.read("stalled-login", None).await.unwrap(), Some(api_key("secret")));
    let result = tokio::time::timeout(Duration::from_secs(5), stalled_refresh).await.expect("stalled refresh settles").unwrap().unwrap();
    assert!(!result.aborted);
}
