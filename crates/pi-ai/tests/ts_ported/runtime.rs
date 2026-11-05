//! Ports of the models-runtime.test.ts cases that `tests/models_runtime.rs` does not cover:
//! credential metadata, pricing tiers, refresh scheduling/cancellation/publication, auth
//! signals, credential-store cancellation and error wrapping.
//!
//! Not portable: "keeps chat reads independent from the all-model catalog" and "swallows provider
//! source failures" exercise provider `getModels`/`getAllModels` callbacks that throw; the Rust
//! `Provider` model sources are infallible (`Vec`), so there is no failure to swallow.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_ai::auth::*;
use pi_ai::utils::models_error::{ModelsError, ModelsErrorCode};
use pi_ai::*;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn test_model(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "test-api".into(),
        provider: provider.into(),
        base_url: "https://example.test/v1".into(),
        input: vec![ModelInputModality::Text],
        context_window: 10000,
        max_tokens: 1000,
        ..Default::default()
    }
}

type Calls = Arc<Mutex<Vec<(Model, StreamOptions)>>>;
type RefreshFn = Arc<dyn Fn(RefreshModelsContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

struct TestProvider {
    id: String,
    auth: ProviderAuth,
    models: Vec<Model>,
    refresh: Option<RefreshFn>,
    calls: Calls,
    base_url: Option<String>,
}

fn test_provider(id: &str, auth: ProviderAuth) -> TestProvider {
    TestProvider { id: id.into(), auth, models: vec![test_model(id, "model-a")], refresh: None, calls: Arc::default(), base_url: None }
}

impl TestProvider {
    fn refreshing(mut self, refresh: impl Fn(RefreshModelsContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync + 'static) -> Self {
        self.refresh = Some(Arc::new(refresh));
        self
    }
}

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.id
    }
    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
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
            Some(f) => f(context).await,
            None => Ok(()),
        }
    }
    fn stream(&self, model: &Model, _context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.calls.lock().push((model.clone(), options));
        let (tx, rx) = AssistantMessageEventStream::new();
        let mut message = AssistantMessage::empty_for(model, utils::now_ms());
        message.content.push(AssistantContent::Text(TextContent::new("ok")));
        tx.push(AssistantMessageEvent::Start { partial: message.clone() });
        tx.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message });
        rx
    }
    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.stream(model, context, options)
    }
}

struct Ambient;
#[async_trait]
impl ApiKeyAuth for Ambient {
    fn name(&self) -> &str {
        "Ambient"
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        Ok(Some(AuthResult::default()))
    }
}

fn ambient() -> ProviderAuth {
    ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None }
}

/// `envKeyAuth`: the stored key wins, otherwise `value`.
struct EnvKey(Option<String>);
#[async_trait]
impl ApiKeyAuth for EnvKey {
    fn name(&self) -> &str {
        "Test API key"
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        let stored = input.credential.as_ref().and_then(|c| c.key.clone());
        let source = if stored.is_some() { "stored" } else { "env" };
        Ok(stored.or(self.0.clone()).map(|key| AuthResult {
            auth: ModelAuth { api_key: Some(key), ..Default::default() },
            env: None,
            source: Some(source.into()),
        }))
    }
}

fn env_key(value: Option<&str>) -> ProviderAuth {
    ProviderAuth { api_key: Some(Arc::new(EnvKey(value.map(str::to_string)))), oauth: None }
}

type RefreshImpl = Arc<dyn Fn(OAuthCredential) -> BoxFuture<'static, anyhow::Result<OAuthCredential>> + Send + Sync>;

struct TestOAuth(RefreshImpl);
#[async_trait]
impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }
    async fn login(&self, _i: ProviderAuthInteraction, _o: Option<LoginOptions>) -> anyhow::Result<OAuthCredential> {
        anyhow::bail!("not used")
    }
    async fn refresh(&self, credential: OAuthCredential, _signal: CancellationToken) -> anyhow::Result<OAuthCredential> {
        (self.0)(credential).await
    }
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        Ok(ModelAuth { api_key: Some(credential.access.clone()), ..Default::default() })
    }
}

fn oauth(refresh: impl Fn(OAuthCredential) -> BoxFuture<'static, anyhow::Result<OAuthCredential>> + Send + Sync + 'static) -> ProviderAuth {
    ProviderAuth { api_key: None, oauth: Some(Arc::new(TestOAuth(Arc::new(refresh)))) }
}

fn oauth_credential(access: &str, refresh: &str, expires: i64) -> Credential {
    Credential::OAuth(OAuthCredential { access: access.into(), refresh: refresh.into(), expires, extra: Default::default() })
}

async fn store(credentials: &Arc<dyn CredentialStore>, id: &str, credential: Credential) {
    credentials.modify(id, Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })), None).await.unwrap();
}

fn memory_credentials() -> Arc<dyn CredentialStore> {
    Arc::new(InMemoryCredentialStore::new())
}

fn models_with(credentials: Option<Arc<dyn CredentialStore>>, models_store: Option<Arc<dyn ModelsStore>>) -> Models {
    create_models(CreateModelsOptions { credentials, models_store, ..Default::default() })
}

fn hi() -> Context {
    Context { messages: vec![Message::User(UserMessage::new("hi", 1))], system_prompt: None, tools: None }
}

fn is_abort(error: &anyhow::Error) -> bool {
    pi_ai::utils::abort::is_abort_error(error)
}

/// Poll `check` until it holds (vitest `vi.waitFor`).
async fn wait_for<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) {
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn enumerates_credential_metadata_without_secrets() {
    let credentials = memory_credentials();
    store(&credentials, "api-provider", Credential::ApiKey(ApiKeyCredential { key: Some("secret".into()), env: None })).await;
    store(&credentials, "oauth-provider", oauth_credential("access", "refresh", utils::now_ms() + 60_000)).await;
    let list = credentials.list(None).await.unwrap();
    assert_eq!(
        serde_json::to_value(list).unwrap(),
        serde_json::json!([{"providerId": "api-provider", "type": "api_key"}, {"providerId": "oauth-provider", "type": "oauth"}])
    );
}

#[test]
fn applies_request_wide_pricing_tiers() {
    let cost: ModelCost = serde_json::from_value(serde_json::json!({"input": 5, "output": 30, "cacheRead": 0.5, "cacheWrite": 6.25,
        "tiers": [{"inputTokensAbove": 272000, "input": 10, "output": 45, "cacheRead": 1, "cacheWrite": 12.5}]}))
    .unwrap();
    let usage = |cache_write: u64| Usage { input: 200000, output: 100000, cache_read: 72000, cache_write, total_tokens: 372000 + cache_write, ..Default::default() };
    let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
    let short = calculate_cost(&cost, &mut usage(0));
    assert!(close(short.input, 1.0) && close(short.output, 3.0) && close(short.cache_read, 0.036) && close(short.cache_write, 0.0), "{short:?}");
    let long = calculate_cost(&cost, &mut usage(1));
    assert!(close(long.input, 2.0) && close(long.output, 4.5) && close(long.cache_read, 0.072) && close(long.cache_write, 0.0000125), "{long:?}");
}

#[tokio::test]
async fn registers_replaces_clears_and_lists_per_provider() {
    let models = models_with(None, None);
    models.set_provider(Arc::new(TestProvider { models: vec![test_model("p1", "m1"), test_model("p1", "m2")], ..test_provider("p1", ambient()) }));
    models.set_provider(Arc::new(TestProvider { models: vec![test_model("p2", "m3")], ..test_provider("p2", ambient()) }));
    let ids = |list: Vec<Model>| list.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(models.get_providers().iter().map(|p| p.id().to_string()).collect::<Vec<_>>(), ["p1", "p2"]);
    assert_eq!(ids(models.get_models(None)), ["m1", "m2", "m3"]);
    assert_eq!(ids(models.get_models(Some("p1"))), ["m1", "m2"]);
    assert!(models.get_models(Some("nope")).is_empty());
    assert_eq!(models.get_model("p2", "m3").map(|m| m.id).as_deref(), Some("m3"));
    assert!(models.get_model("p2", "missing").is_none());

    let replacement: Arc<dyn Provider> = Arc::new(test_provider("p1", ambient()));
    models.set_provider(replacement.clone());
    assert!(Arc::ptr_eq(&models.get_provider("p1").unwrap(), &replacement));
    assert_eq!(models.get_providers().len(), 2);
    models.delete_provider("p1");
    assert!(models.get_provider("p1").is_none());
    models.clear_providers();
    assert!(models.get_providers().is_empty());
}

#[tokio::test]
async fn restricts_refresh_work_to_selected_providers() {
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let models = models_with(None, None);
    for id in ["one", "two"] {
        let calls = calls.clone();
        models.set_provider(Arc::new(test_provider(id, ambient()).refreshing(move |context| {
            calls.lock().push(format!("{id}:{}", if context.allow_network { "network" } else { "cache" }));
            Box::pin(async { Ok(()) })
        })));
    }
    let result = models.refresh(ModelsRefreshOptions { providers: Some(vec!["two".into(), "unknown".into()]), ..Default::default() }).await;
    assert!(result.errors.is_empty());
    assert_eq!(*calls.lock(), ["two:cache", "two:network"]);
}

struct BlockedAuth {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ApiKeyAuth for BlockedAuth {
    fn name(&self) -> &str {
        "Blocked auth"
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(Some(AuthResult { auth: ModelAuth { api_key: Some("key".into()), ..Default::default() }, env: None, source: None }))
    }
}

#[tokio::test]
async fn restores_cached_models_before_waiting_for_network_auth() {
    let models_store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
    models_store.write("dynamic", ModelsStoreEntry { models: vec![AnyModel::Chat(test_model("dynamic", "cached"))], ..Default::default() }, None).await.unwrap();
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = create_provider(CreateProviderOptions {
        id: "dynamic".into(),
        auth: ProviderAuth { api_key: Some(Arc::new(BlockedAuth { started: started.clone(), release: release.clone() })), oauth: None },
        models: vec![],
        fetch_models: Some(Arc::new(|_ctx| Box::pin(async { anyhow::bail!("must not fetch") }))),
        api: Some(ProviderApi::Single(pi_ai::api::registry::builtin_api("test-api"))),
        ..Default::default()
    })
    .unwrap();
    let models = models_with(None, Some(models_store));
    models.set_provider(provider);
    let signal = CancellationToken::new();
    let pending = {
        let models = models.clone();
        let signal = signal.clone();
        tokio::spawn(async move { models.refresh(ModelsRefreshOptions { providers: Some(vec!["dynamic".into()]), signal: Some(signal), ..Default::default() }).await })
    };
    started.notified().await;
    assert!(models.get_model("dynamic", "cached").is_some());
    signal.cancel();
    assert!(pending.await.unwrap().aborted);
    release.notify_one();
}

/// A models store over one entry, recording the signals it is given.
#[derive(Default)]
struct RecordingStore {
    entry: Mutex<Option<ModelsStoreEntry>>,
    signals: Mutex<Vec<Option<CancellationToken>>>,
}

#[async_trait]
impl ModelsStore for RecordingStore {
    async fn read(&self, _id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<ModelsStoreEntry>> {
        self.signals.lock().push(signal.cloned());
        Ok(self.entry.lock().clone())
    }
    async fn write(&self, _id: &str, entry: ModelsStoreEntry, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        self.signals.lock().push(signal.cloned());
        *self.entry.lock() = Some(entry);
        Ok(())
    }
    async fn delete(&self, _id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        self.signals.lock().push(signal.cloned());
        *self.entry.lock() = None;
        Ok(())
    }
}

#[tokio::test]
async fn providers_choose_persistent_deletion_and_ephemeral_publication() {
    let store = Arc::new(RecordingStore::default());
    *store.entry.lock() = Some(ModelsStoreEntry { models: vec![AnyModel::Chat(test_model("dynamic", "stored"))], ..Default::default() });
    let state = Arc::new(Mutex::new("initial"));
    let models = models_with(None, Some(store.clone()));
    let (sink, entry_view) = (state.clone(), store.clone());
    models.set_provider(Arc::new(test_provider("dynamic", ambient()).refreshing(move |context| {
        let (sink, entry_view) = (sink.clone(), entry_view.clone());
        Box::pin(async move {
            assert_eq!(context.stored.as_ref().and_then(|s| s.models.first()).map(|m| m.id().to_string()).as_deref(), Some("stored"));
            let deleted = sink.clone();
            let view = entry_view.clone();
            (context.publish)(ModelsPublication {
                persist: Some(None),
                update: Some(Box::new(move || {
                    assert!(view.entry.lock().is_none());
                    *deleted.lock() = "deleted";
                })),
            })
            .await?;
            (context.publish)(ModelsPublication { persist: None, update: Some(Box::new(move || *sink.lock() = "ephemeral")) }).await?;
            Ok(())
        })
    })));
    let result = models.refresh(ModelsRefreshOptions { allow_network: Some(false), ..Default::default() }).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(store.entry.lock().is_none());
    assert_eq!(*state.lock(), "ephemeral");
}

#[tokio::test]
async fn passes_effective_credentials_and_force_while_skipping_unconfigured() {
    let seen: Arc<Mutex<Option<(Option<Credential>, Option<bool>)>>> = Arc::default();
    let unconfigured = Arc::new(Mutex::new(0));
    let models = models_with(None, None);
    let sink = seen.clone();
    models.set_provider(Arc::new(test_provider("configured", env_key(Some("ambient-key"))).refreshing(move |context| {
        if context.allow_network {
            *sink.lock() = Some((context.credential.clone(), context.force));
        }
        Box::pin(async { Ok(()) })
    })));
    let counter = unconfigured.clone();
    models.set_provider(Arc::new(test_provider("unconfigured", env_key(None)).refreshing(move |context| {
        if context.allow_network {
            *counter.lock() += 1;
        }
        Box::pin(async { Ok(()) })
    })));
    models.refresh(ModelsRefreshOptions { force: Some(true), ..Default::default() }).await;
    let (credential, force) = seen.lock().clone().expect("configured provider refreshed");
    assert_eq!(credential, Some(Credential::ApiKey(ApiKeyCredential { key: Some("ambient-key".into()), env: None })));
    assert_eq!(force, Some(true));
    assert_eq!(*unconfigured.lock(), 0);
}

#[tokio::test]
async fn passes_the_registered_base_url_to_refresh_models() {
    let base_urls: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let sink = base_urls.clone();
    let mut provider = test_provider("dynamic", ambient()).refreshing(move |context| {
        sink.lock().push(context.base_url.clone());
        Box::pin(async { Ok(()) })
    });
    // A models.json override wraps the built-in provider with a regional base URL.
    provider.base_url = Some("https://regional.example.test/v1".into());
    let models = models_with(None, None);
    models.set_provider(Arc::new(provider));
    models.refresh(ModelsRefreshOptions::default()).await;
    let regional = Some("https://regional.example.test/v1".to_string());
    assert_eq!(*base_urls.lock(), [regional.clone(), regional]);
}

#[tokio::test]
async fn refreshes_expired_oauth_before_refreshing_models() {
    let credentials = memory_credentials();
    store(&credentials, "oauth-dynamic", oauth_credential("expired", "refresh", 0)).await;
    let seen: Arc<Mutex<Option<Credential>>> = Arc::default();
    let models = models_with(Some(credentials.clone()), None);
    let sink = seen.clone();
    let auth = oauth(|_| Box::pin(async { Ok(OAuthCredential { access: "fresh".into(), refresh: "rotated".into(), expires: utils::now_ms() + 60_000, extra: Default::default() }) }));
    models.set_provider(Arc::new(test_provider("oauth-dynamic", auth).refreshing(move |context| {
        if context.allow_network {
            *sink.lock() = context.credential.clone();
        }
        Box::pin(async { Ok(()) })
    })));
    assert!(models.refresh(ModelsRefreshOptions::default()).await.errors.is_empty());
    let credential = seen.lock().clone().and_then(|c| c.as_oauth().cloned()).expect("oauth credential");
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), ("fresh", "rotated"));
    let stored = credentials.read("oauth-dynamic", None).await.unwrap().unwrap().as_oauth().cloned().unwrap();
    assert_eq!((stored.access.as_str(), stored.refresh.as_str()), ("fresh", "rotated"));
}

/// The provider rotates the refresh token while the caller cancels: the rotation must persist.
fn rotating_and_cancelling(signal: CancellationToken) -> ProviderAuth {
    oauth(move |credential| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.cancel();
            Ok(OAuthCredential { access: "new".into(), refresh: "new-refresh".into(), expires: utils::now_ms() + 60_000, ..credential })
        })
    })
}

async fn refresh_token_of(credentials: &Arc<dyn CredentialStore>, id: &str) -> Option<String> {
    credentials.read(id, None).await.ok().flatten().and_then(|c| c.as_oauth().map(|o| o.refresh.clone()))
}

#[tokio::test]
async fn persists_oauth_refresh_started_before_model_refresh_was_cancelled() {
    let credentials = memory_credentials();
    store(&credentials, "p1", oauth_credential("old", "old-refresh", 0)).await;
    let signal = CancellationToken::new();
    let models = models_with(Some(credentials.clone()), None);
    models.set_provider(Arc::new(test_provider("p1", rotating_and_cancelling(signal.clone())).refreshing(|_| Box::pin(async { Ok(()) }))));
    assert!(models.refresh(ModelsRefreshOptions { signal: Some(signal), ..Default::default() }).await.aborted);
    wait_for(|| async { refresh_token_of(&credentials, "p1").await.as_deref() == Some("new-refresh") }).await;
}

#[tokio::test]
async fn persists_oauth_refresh_started_before_the_request_was_cancelled() {
    // Radius bug report 01a10855-43d8-7447-a710-2acf5ee0b2ee: refresh_token_invalidated after a cancelled refresh.
    let credentials = memory_credentials();
    store(&credentials, "p1", oauth_credential("old", "old-refresh", 0)).await;
    let signal = CancellationToken::new();
    let models = models_with(Some(credentials.clone()), None);
    models.set_provider(Arc::new(test_provider("p1", rotating_and_cancelling(signal.clone()))));
    let error = models.get_auth("p1", AuthResolutionOverrides { signal: Some(signal), ..Default::default() }).await.unwrap_err();
    assert!(is_abort(&error), "{error:#}");
    wait_for(|| async { refresh_token_of(&credentials, "p1").await.as_deref() == Some("new-refresh") }).await;
}

#[tokio::test]
async fn providers_get_a_live_signal_and_store_waits_use_it() {
    let store = Arc::new(RecordingStore::default());
    let provider_signal: Arc<Mutex<Option<CancellationToken>>> = Arc::default();
    let sink = provider_signal.clone();
    let models = models_with(None, Some(store.clone()));
    models.set_provider(Arc::new(test_provider("dynamic", env_key(Some("key"))).refreshing(move |context| {
        assert!(!context.signal.is_cancelled());
        *sink.lock() = Some(context.signal.clone());
        Box::pin(async move {
            if !context.allow_network {
                return Ok(());
            }
            (context.publish)(ModelsPublication {
                persist: Some(Some(ModelsStoreEntry { models: vec![AnyModel::Chat(test_model("dynamic", "fresh"))], ..Default::default() })),
                update: None,
            })
            .await?;
            Ok(())
        })
    })));
    let result = models.refresh(ModelsRefreshOptions { providers: Some(vec!["dynamic".into()]), ..Default::default() }).await;
    assert!(!result.aborted);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let signals = store.signals.lock().clone();
    assert_eq!(signals.len(), 3);
    // Every store call got the provider's signal: cancelling it reaches each of them.
    provider_signal.lock().clone().unwrap().cancel();
    assert!(signals.iter().all(|s| s.as_ref().is_some_and(CancellationToken::is_cancelled)));
}

#[tokio::test]
async fn returns_aborted_state_without_reporting_cancellation_as_error() {
    let signal = CancellationToken::new();
    let caller = signal.clone();
    let models = models_with(None, None);
    models.set_provider(Arc::new(test_provider("dynamic", ambient()).refreshing(move |_context| {
        caller.cancel();
        Box::pin(async { Ok(()) })
    })));
    let result = models.refresh(ModelsRefreshOptions { signal: Some(signal), ..Default::default() }).await;
    assert!(result.aborted);
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn stops_waiting_on_abort_when_a_provider_ignores_its_signal() {
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let calls = Arc::new(Mutex::new(0));
    let models = models_with(None, None);
    let (s, r, c) = (started.clone(), release.clone(), calls.clone());
    models.set_provider(Arc::new(test_provider("dynamic", ambient()).refreshing(move |_| {
        let (s, r) = (s.clone(), r.clone());
        let first = {
            let mut calls = c.lock();
            *calls += 1;
            *calls == 1
        };
        Box::pin(async move {
            if !first {
                return Ok(());
            }
            s.notify_one();
            r.notified().await;
            anyhow::bail!("late provider failure")
        })
    })));
    let signal = CancellationToken::new();
    let pending = {
        let models = models.clone();
        let signal = signal.clone();
        tokio::spawn(async move { models.refresh(ModelsRefreshOptions { signal: Some(signal), ..Default::default() }).await })
    };
    started.notified().await;
    signal.cancel();
    let result = pending.await.unwrap();
    assert!(result.aborted);
    assert!(result.errors.is_empty());
    release.notify_one();
    tokio::task::yield_now().await;
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn rejects_late_publication_from_a_superseded_provider() {
    let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
    let state = Arc::new(Mutex::new(String::from("initial")));
    let calls = Arc::new(Mutex::new(0));
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let models = models_with(None, Some(store.clone()));
    let (sink, c, s, r) = (state.clone(), calls.clone(), started.clone(), release.clone());
    models.set_provider(Arc::new(test_provider("dynamic", ambient()).refreshing(move |context| {
        let (sink, s, r) = (sink.clone(), s.clone(), r.clone());
        let current = if context.allow_network {
            let mut calls = c.lock();
            *calls += 1;
            Some(*calls)
        } else {
            None
        };
        Box::pin(async move {
            let Some(current) = current else { return Ok(()) };
            if current == 1 {
                s.notify_one();
                r.notified().await;
            }
            let value = format!("generation-{current}");
            let model = test_model("dynamic", &value);
            let _ = (context.publish)(ModelsPublication {
                persist: Some(Some(ModelsStoreEntry { models: vec![AnyModel::Chat(model)], ..Default::default() })),
                update: Some(Box::new(move || *sink.lock() = value)),
            })
            .await;
            Ok(())
        })
    })));
    let first = {
        let models = models.clone();
        tokio::spawn(async move { models.refresh(ModelsRefreshOptions { providers: Some(vec!["dynamic".into()]), ..Default::default() }).await })
    };
    started.notified().await;
    models.refresh(ModelsRefreshOptions { providers: Some(vec!["dynamic".into()]), ..Default::default() }).await;
    first.await.unwrap();
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(*state.lock(), "generation-2");
    let stored = store.read("dynamic", None).await.unwrap().unwrap();
    assert_eq!(stored.models[0].id(), "generation-2");
}

/// Records the signal of every auth callback.
struct SignalAuth(Arc<Mutex<Vec<CancellationToken>>>);

#[async_trait]
impl ApiKeyAuth for SignalAuth {
    fn name(&self) -> &str {
        "Signal auth"
    }
    fn supports_login(&self) -> bool {
        true
    }
    async fn login(&self, interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        self.0.lock().push(interaction.signal.clone());
        Ok(ApiKeyCredential { key: Some("saved".into()), env: None })
    }
    fn supports_check(&self) -> bool {
        true
    }
    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        self.0.lock().push(input.signal.clone());
        Ok(Some(AuthCheck { source: None, auth_type: AuthType::ApiKey }))
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        self.0.lock().push(input.signal.clone());
        Ok(Some(AuthResult { auth: ModelAuth { api_key: Some("resolved".into()), ..Default::default() }, env: None, source: None }))
    }
}

struct Unused;
#[async_trait]
impl AuthInteraction for Unused {
    async fn prompt(&self, _p: AuthPrompt) -> anyhow::Result<String> {
        Ok("unused".into())
    }
    fn notify(&self, _e: AuthEvent) {}
}

#[tokio::test]
async fn passes_caller_signals_to_provider_auth_callbacks() {
    let received: Arc<Mutex<Vec<CancellationToken>>> = Arc::default();
    let models = models_with(None, None);
    models.set_provider(Arc::new(test_provider("p1", ProviderAuth { api_key: Some(Arc::new(SignalAuth(received.clone()))), oauth: None })));
    let signal = CancellationToken::new();
    models.check_auth("p1", Some(signal.clone())).await.unwrap();
    models.get_auth("p1", AuthResolutionOverrides { signal: Some(signal.clone()), ..Default::default() }).await.unwrap();
    models.login("p1", AuthType::ApiKey, Arc::new(Unused), Some(signal.clone()), None).await.unwrap();
    let received = received.lock().clone();
    assert_eq!(received.len(), 3);
    // Each callback got the caller's signal: cancelling it reaches every recorded token.
    signal.cancel();
    assert!(received.iter().all(CancellationToken::is_cancelled));
}

struct NonCooperativeAuth {
    check_started: Arc<Notify>,
    resolve_started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ApiKeyAuth for NonCooperativeAuth {
    fn name(&self) -> &str {
        "Blocked auth"
    }
    fn supports_check(&self) -> bool {
        true
    }
    async fn check(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        self.check_started.notify_one();
        self.release.notified().await;
        Ok(Some(AuthCheck { source: None, auth_type: AuthType::ApiKey }))
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        self.resolve_started.notify_one();
        self.release.notified().await;
        Ok(Some(AuthResult { auth: ModelAuth { api_key: Some("key".into()), ..Default::default() }, env: None, source: None }))
    }
}

#[tokio::test]
async fn stops_waiting_for_non_cooperative_auth_callbacks() {
    let (check_started, resolve_started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()), Arc::new(Notify::new()));
    let auth = NonCooperativeAuth { check_started: check_started.clone(), resolve_started: resolve_started.clone(), release: release.clone() };
    let models = models_with(None, None);
    models.set_provider(Arc::new(test_provider("p1", ProviderAuth { api_key: Some(Arc::new(auth)), oauth: None })));

    let signal = CancellationToken::new();
    let available = {
        let models = models.clone();
        let signal = signal.clone();
        tokio::spawn(async move { models.get_available(None, Some(signal)).await })
    };
    check_started.notified().await;
    signal.cancel();
    let error = available.await.unwrap().unwrap_err();
    assert!(is_abort(&error), "{error:#}");

    let signal = CancellationToken::new();
    let auth = {
        let models = models.clone();
        let signal = signal.clone();
        tokio::spawn(async move { models.get_auth("p1", AuthResolutionOverrides { signal: Some(signal), ..Default::default() }).await })
    };
    resolve_started.notified().await;
    signal.cancel();
    let error = auth.await.unwrap().unwrap_err();
    assert!(is_abort(&error), "{error:#}");
    release.notify_waiters();
}

#[tokio::test]
async fn cancels_queued_credential_mutations_without_running_them() {
    let credentials = memory_credentials();
    let release = Arc::new(Notify::new());
    let second_ran = Arc::new(Mutex::new(false));
    let first = {
        let credentials = credentials.clone();
        let release = release.clone();
        tokio::spawn(async move {
            credentials
                .modify(
                    "p1",
                    Box::new(move |_| {
                        Box::pin(async move {
                            release.notified().await;
                            Ok(Some(Credential::ApiKey(ApiKeyCredential { key: Some("first".into()), env: None })))
                        })
                    }),
                    None,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    let signal = CancellationToken::new();
    let second = {
        let credentials = credentials.clone();
        let ran = second_ran.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            credentials
                .modify(
                    "p1",
                    Box::new(move |_| {
                        *ran.lock() = true;
                        Box::pin(async { Ok(Some(Credential::ApiKey(ApiKeyCredential { key: Some("second".into()), env: None }))) })
                    }),
                    Some(&signal),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    signal.cancel();
    let error = second.await.unwrap().unwrap_err();
    assert!(is_abort(&error), "{error:#}");
    release.notify_one();
    first.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!*second_ran.lock());
    assert_eq!(
        credentials.read("p1", None).await.unwrap(),
        Some(Credential::ApiKey(ApiKeyCredential { key: Some("first".into()), env: None }))
    );
}

#[tokio::test]
async fn model_and_provider_overloads_resolve_the_same_auth() {
    let models = models_with(Some(memory_credentials()), None);
    models.set_provider(Arc::new(test_provider("p1", env_key(Some("env-key")))));
    let model = AnyModel::Chat(test_model("p1", "model-a"));
    let key = |r: Option<AuthResult>| r.and_then(|r| r.auth.api_key);
    assert_eq!(key(models.get_auth_for_model(&model, Default::default()).await.unwrap()).as_deref(), Some("env-key"));
    assert_eq!(key(models.get_auth("p1", Default::default()).await.unwrap()).as_deref(), Some("env-key"));
    let explicit = AuthResolutionOverrides { api_key: Some("explicit-key".into()), ..Default::default() };
    assert_eq!(key(models.get_auth_for_model(&model, explicit).await.unwrap()).as_deref(), Some("explicit-key"));
    let available: Vec<String> = models.get_available(Some("p1"), None).await.unwrap().into_iter().map(|m| m.provider).collect();
    assert_eq!(available, ["p1"]);
}

#[tokio::test]
async fn serializes_concurrent_oauth_refreshes() {
    let credentials = memory_credentials();
    store(&credentials, "p1", oauth_credential("old", "r1", 0)).await;
    let refreshes = Arc::new(Mutex::new(0));
    let counter = refreshes.clone();
    let models = models_with(Some(credentials), None);
    models.set_provider(Arc::new(test_provider(
        "p1",
        oauth(move |_| {
            let counter = counter.clone();
            Box::pin(async move {
                let n = {
                    let mut n = counter.lock();
                    *n += 1;
                    *n
                };
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(OAuthCredential { access: format!("new-{n}"), refresh: "r2".into(), expires: utils::now_ms() + 3_600_000, extra: Default::default() })
            })
        }),
    )));
    let (a, b) = tokio::join!(models.get_auth("p1", Default::default()), models.get_auth("p1", Default::default()));
    assert_eq!(*refreshes.lock(), 1);
    assert_eq!(a.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    assert_eq!(b.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
}

/// Wraps a store, counting `modify` calls and optionally failing reads or modifies.
struct WrappedStore {
    base: InMemoryCredentialStore,
    modifies: Mutex<u32>,
    fail_read: bool,
    fail_modify: bool,
    stored_oauth: bool,
}

#[async_trait]
impl CredentialStore for WrappedStore {
    async fn read(&self, id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        if self.fail_read {
            anyhow::bail!("disk on fire");
        }
        if self.stored_oauth {
            return Ok(Some(oauth_credential("old", "r", 0)));
        }
        self.base.read(id, signal).await
    }
    async fn list(&self, signal: Option<&CancellationToken>) -> anyhow::Result<Vec<CredentialInfo>> {
        self.base.list(signal).await
    }
    async fn modify(&self, id: &str, f: ModifyCredentialFn, signal: Option<&CancellationToken>) -> anyhow::Result<Option<Credential>> {
        *self.modifies.lock() += 1;
        if self.fail_modify {
            anyhow::bail!("disk on fire");
        }
        self.base.modify(id, f, signal).await
    }
    async fn delete(&self, id: &str, signal: Option<&CancellationToken>) -> anyhow::Result<()> {
        self.base.delete(id, signal).await
    }
}

fn wrapped(fail_read: bool, fail_modify: bool, stored_oauth: bool) -> Arc<WrappedStore> {
    Arc::new(WrappedStore { base: InMemoryCredentialStore::new(), modifies: Mutex::new(0), fail_read, fail_modify, stored_oauth })
}

fn identity_oauth() -> ProviderAuth {
    oauth(|credential| Box::pin(async move { Ok(credential) }))
}

#[tokio::test]
async fn valid_oauth_tokens_resolve_without_modify() {
    let store = wrapped(false, false, false);
    store
        .base
        .modify("p1", Box::new(|_| Box::pin(async { Ok(Some(oauth_credential("valid", "r", utils::now_ms() + 600_000))) })), None)
        .await
        .unwrap();
    let models = models_with(Some(store.clone()), None);
    models.set_provider(Arc::new(test_provider("p1", identity_oauth())));
    assert_eq!(models.get_auth("p1", Default::default()).await.unwrap().unwrap().auth.api_key.as_deref(), Some("valid"));
    assert_eq!(*store.modifies.lock(), 0);
}

fn code_of(error: &anyhow::Error) -> ModelsErrorCode {
    error.downcast_ref::<ModelsError>().unwrap_or_else(|| panic!("not a ModelsError: {error:#}")).code
}

#[tokio::test]
async fn wraps_credential_store_and_api_key_failures_in_models_error() {
    let models = models_with(Some(wrapped(true, false, false)), None);
    models.set_provider(Arc::new(test_provider("p1", env_key(Some("env-key")))));
    assert_eq!(code_of(&models.get_auth("p1", Default::default()).await.unwrap_err()), ModelsErrorCode::Auth);

    let models = models_with(Some(wrapped(false, true, true)), None);
    models.set_provider(Arc::new(test_provider("p1", identity_oauth())));
    assert_eq!(code_of(&models.get_auth("p1", Default::default()).await.unwrap_err()), ModelsErrorCode::Auth);

    struct Failing;
    #[async_trait]
    impl ApiKeyAuth for Failing {
        fn name(&self) -> &str {
            "Failing"
        }
        async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
            anyhow::bail!("nope")
        }
    }
    let models = models_with(None, None);
    models.set_provider(Arc::new(test_provider("p1", ProviderAuth { api_key: Some(Arc::new(Failing)), oauth: None })));
    assert_eq!(code_of(&models.get_auth("p1", Default::default()).await.unwrap_err()), ModelsErrorCode::Auth);
}

#[tokio::test]
async fn keeps_the_underlying_reason_in_wrapped_oauth_refresh_errors() {
    let credentials = memory_credentials();
    store(&credentials, "p1", oauth_credential("old", "r", 0)).await;
    let models = models_with(Some(credentials), None);
    models.set_provider(Arc::new(test_provider("p1", oauth(|_| Box::pin(async { anyhow::bail!("token refresh failed (400): invalid_grant") })))));
    let error = models.get_auth("p1", Default::default()).await.unwrap_err();
    assert!(error.to_string().contains("OAuth refresh failed for p1: token refresh failed (400): invalid_grant"), "{error}");
}

/// `Scoped`: needs the explicit key and an ACCOUNT_ID from the credential env or the auth context.
struct Scoped;
#[async_trait]
impl ApiKeyAuth for Scoped {
    fn name(&self) -> &str {
        "Scoped"
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        let account = match input.credential.as_ref().and_then(|c| c.env.as_ref()).and_then(|e| e.get("ACCOUNT_ID").cloned()) {
            Some(account) => Some(account),
            None => input.ctx.env("ACCOUNT_ID").await,
        };
        let (Some(key), Some(account)) = (input.credential.as_ref().and_then(|c| c.key.clone()), account) else { return Ok(None) };
        let mut env = ProviderEnv::new();
        env.insert("ACCOUNT_ID".into(), account.clone());
        Ok(Some(AuthResult {
            auth: ModelAuth { api_key: Some(key), base_url: Some(format!("https://example.test/{account}")), ..Default::default() },
            env: Some(env),
            source: None,
        }))
    }
}

#[tokio::test]
async fn uses_explicit_request_api_key_and_env_during_auth_resolution() {
    let provider = test_provider("p1", ProviderAuth { api_key: Some(Arc::new(Scoped)), oauth: None });
    let calls = provider.calls.clone();
    let models = models_with(None, None);
    models.set_provider(Arc::new(provider));
    let mut env = ProviderEnv::new();
    env.insert("ACCOUNT_ID".into(), "acct".into());
    models
        .complete_simple(&test_model("p1", "model-a"), &hi(), StreamOptions { api_key: Some("explicit-key".into()), env: Some(env.clone()), ..Default::default() })
        .await;
    let (model, options) = calls.lock()[0].clone();
    assert_eq!(model.base_url, "https://example.test/acct");
    assert_eq!(options.api_key.as_deref(), Some("explicit-key"));
    assert_eq!(options.env, Some(env));
}

#[tokio::test]
async fn model_auth_carries_model_headers() {
    let models = models_with(None, None);
    models.set_provider(Arc::new(test_provider("p1", env_key(Some("key")))));
    let mut model = test_model("p1", "model-a");
    let mut headers = indexmap::IndexMap::new();
    headers.insert("x-model".to_string(), "model".to_string());
    headers.insert("x-shared".to_string(), "model".to_string());
    model.headers = Some(headers);
    assert!(models.get_auth("p1", Default::default()).await.unwrap().unwrap().auth.headers.is_none());
    let auth = models.get_auth_for_model(&AnyModel::Chat(model), Default::default()).await.unwrap().unwrap();
    let mut expected = ProviderHeaders::new();
    expected.insert("x-model".into(), Some("model".into()));
    expected.insert("x-shared".into(), Some("model".into()));
    assert_eq!(auth.auth.headers, Some(expected));
}


/// The Radius report again, with the cancellation arriving while the refresh request is still in
/// flight (the TS promise keeps running after the caller stops waiting).
#[tokio::test]
async fn persists_an_in_flight_oauth_refresh_after_the_caller_cancels() {
    let credentials = memory_credentials();
    store(&credentials, "p1", oauth_credential("old", "old-refresh", 0)).await;
    let refreshing = Arc::new(Notify::new());
    let notify = refreshing.clone();
    let models = models_with(Some(credentials.clone()), None);
    models.set_provider(Arc::new(test_provider(
        "p1",
        oauth(move |credential| {
            let notify = notify.clone();
            Box::pin(async move {
                notify.notify_one();
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(OAuthCredential { access: "new".into(), refresh: "new-refresh".into(), expires: utils::now_ms() + 60_000, ..credential })
            })
        }),
    )));
    let signal = CancellationToken::new();
    let pending = {
        let models = models.clone();
        let signal = signal.clone();
        tokio::spawn(async move { models.get_auth("p1", AuthResolutionOverrides { signal: Some(signal), ..Default::default() }).await })
    };
    refreshing.notified().await;
    signal.cancel();
    let error = pending.await.unwrap().unwrap_err();
    assert!(is_abort(&error), "{error:#}");
    wait_for(|| async { refresh_token_of(&credentials, "p1").await.as_deref() == Some("new-refresh") }).await;
}
