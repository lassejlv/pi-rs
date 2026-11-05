//! Port of `test/models-runtime.test.ts` (selected cases).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use pi_ai::auth::*;
use pi_ai::*;
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
type RefreshFn = Arc<dyn Fn(RefreshModelsContext, Arc<RwLock<Vec<Model>>>) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

struct TestProvider {
    id: String,
    auth: ProviderAuth,
    models: Arc<RwLock<Vec<Model>>>,
    refresh: Option<RefreshFn>,
    calls: Calls,
}

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.id
    }
    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.read().clone()
    }
    fn supports_refresh(&self) -> bool {
        self.refresh.is_some()
    }
    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        match &self.refresh {
            Some(f) => f(context, self.models.clone()).await,
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

/// Env-key auth: stored key wins (source "stored"), otherwise `value` (source "env").
struct EnvKey {
    value: Option<String>,
    login_key: Option<String>,
}

#[async_trait]
impl ApiKeyAuth for EnvKey {
    fn name(&self) -> &str {
        "Env"
    }
    fn supports_login(&self) -> bool {
        self.login_key.is_some()
    }
    async fn login(&self, _i: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        Ok(ApiKeyCredential { key: self.login_key.clone(), env: None })
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        if let Some(key) = input.credential.as_ref().and_then(|c| c.key.clone()) {
            return Ok(Some(AuthResult {
                auth: ModelAuth { api_key: Some(key), ..Default::default() },
                env: None,
                source: Some("stored".into()),
            }));
        }
        Ok(self.value.clone().map(|v| AuthResult {
            auth: ModelAuth { api_key: Some(v), ..Default::default() },
            env: None,
            source: Some("env".into()),
        }))
    }
}

type RefreshImpl = Arc<dyn Fn(OAuthCredential) -> anyhow::Result<OAuthCredential> + Send + Sync>;

struct TestOAuth {
    refresh: RefreshImpl,
    refreshes: Arc<Mutex<u32>>,
}

#[async_trait]
impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }
    async fn login(&self, _i: ProviderAuthInteraction, _o: Option<LoginOptions>) -> anyhow::Result<OAuthCredential> {
        anyhow::bail!("no login")
    }
    async fn refresh(&self, credential: OAuthCredential, _signal: CancellationToken) -> anyhow::Result<OAuthCredential> {
        *self.refreshes.lock() += 1;
        (self.refresh)(credential)
    }
    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        Ok(ModelAuth { api_key: Some(credential.access.clone()), ..Default::default() })
    }
}

fn oauth_with(refresh: RefreshImpl) -> (Arc<dyn OAuthAuth>, Arc<Mutex<u32>>) {
    let refreshes = Arc::new(Mutex::new(0));
    (Arc::new(TestOAuth { refresh, refreshes: refreshes.clone() }), refreshes)
}

fn renewing() -> RefreshImpl {
    Arc::new(|mut c: OAuthCredential| {
        c.access = "new-token".into();
        c.expires = utils::now_ms() + 3_600_000;
        Ok(c)
    })
}

fn provider(id: &str, auth: ProviderAuth) -> (Arc<dyn Provider>, Calls) {
    let calls: Calls = Arc::default();
    let p = TestProvider { id: id.into(), auth, models: Arc::new(RwLock::new(vec![test_model(id, "model-a")])), refresh: None, calls: calls.clone() };
    (Arc::new(p), calls)
}

fn oauth_credential(access: &str, expires: i64) -> Credential {
    Credential::OAuth(OAuthCredential { access: access.into(), refresh: "r".into(), expires, extra: Default::default() })
}

async fn store_credential(store: &Arc<dyn CredentialStore>, id: &str, credential: Credential) {
    store.modify(id, Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })), None).await.unwrap();
}

fn ctx() -> Context {
    Context { messages: vec![Message::User(UserMessage::new("hi", 1))], ..Default::default() }
}

#[tokio::test]
async fn registers_replaces_deletes_and_lists() {
    let models = create_models(CreateModelsOptions::default());
    let (p1, _) = provider("p1", ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None });
    models.set_provider(p1);
    assert_eq!(models.get_providers().len(), 1);
    assert!(models.get_model("p1", "model-a").is_some());
    assert!(models.get_model("p1", "missing").is_none());
    assert_eq!(models.get_models(None).len(), 1);
    models.delete_provider("p1");
    assert!(models.get_providers().is_empty());
}

#[tokio::test]
async fn refresh_updates_dynamic_providers_and_reports_failures() {
    let models = create_models(CreateModelsOptions::default());
    let refreshes = Arc::new(Mutex::new(0));
    let counter = refreshes.clone();
    let dynamic = TestProvider {
        id: "dyn".into(),
        auth: ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None },
        models: Arc::new(RwLock::new(vec![test_model("dyn", "before")])),
        refresh: Some(Arc::new(move |context: RefreshModelsContext, list: Arc<RwLock<Vec<Model>>>| {
            let counter = counter.clone();
            Box::pin(async move {
                if !context.allow_network {
                    return Ok(());
                }
                *counter.lock() += 1;
                (context.publish)(ModelsPublication {
                    persist: None,
                    update: Some(Box::new(move || *list.write() = vec![test_model("dyn", "after")])),
                })
                .await?;
                Ok(())
            })
        })),
        calls: Arc::default(),
    };
    models.set_provider(Arc::new(dynamic));
    assert!(models.get_model("dyn", "before").is_some());
    let first = models.refresh(ModelsRefreshOptions::default()).await;
    assert!(first.errors.is_empty());
    assert_eq!(*refreshes.lock(), 1);
    assert!(models.get_model("dyn", "after").is_some());
    assert!(models.get_model("dyn", "before").is_none());

    let flaky = TestProvider {
        id: "flaky".into(),
        auth: ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None },
        models: Arc::default(),
        refresh: Some(Arc::new(|context: RefreshModelsContext, _| {
            Box::pin(async move { if context.allow_network { anyhow::bail!("fetch failed") } else { Ok(()) } })
        })),
        calls: Arc::default(),
    };
    models.set_provider(Arc::new(flaky));
    let second = models.refresh(ModelsRefreshOptions::default()).await;
    assert_eq!(*refreshes.lock(), 2);
    assert_eq!(second.errors.get("flaky").map(String::as_str), Some("fetch failed"));

    let only = models.refresh(ModelsRefreshOptions { providers: Some(vec!["dyn".into(), "unknown".into()]), ..Default::default() }).await;
    assert!(only.errors.is_empty());
    assert_eq!(*refreshes.lock(), 3);
}

fn failing_provider(id: &str, delay_ms: u64) -> TestProvider {
    let message = format!("{id} failed");
    TestProvider {
        id: id.into(),
        auth: ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None },
        models: Arc::default(),
        refresh: Some(Arc::new(move |context: RefreshModelsContext, _| {
            let message = message.clone();
            Box::pin(async move {
                if !context.allow_network {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                anyhow::bail!(message)
            })
        })),
        calls: Arc::default(),
    }
}

/// TS returns a `Map`, whose keys iterate in insertion (failure) order; callers list them in that order.
#[tokio::test]
async fn refresh_errors_keep_failure_order() {
    let models = create_models(CreateModelsOptions::default());
    for (id, delay) in [("zeta", 40), ("alpha", 0), ("mid", 20), ("beta", 60)] {
        models.set_provider(Arc::new(failing_provider(id, delay)));
    }
    let result = models.refresh(ModelsRefreshOptions::default()).await;
    let keys: Vec<&str> = result.errors.keys().map(String::as_str).collect();
    assert_eq!(keys, ["alpha", "mid", "zeta", "beta"]);
    assert_eq!(result.errors["mid"], "mid failed");
}

#[tokio::test]
async fn create_provider_persists_and_restores_dynamic_catalogs() {
    let store: Arc<dyn ModelsStore> = Arc::new(InMemoryModelsStore::new());
    let fetches = Arc::new(Mutex::new(0));
    let counter = fetches.clone();
    let make = || {
        let counter = counter.clone();
        create_provider(CreateProviderOptions {
            id: "dyn".into(),
            auth: ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None },
            models: vec![AnyModel::Chat(test_model("dyn", "base"))],
            fetch_models: Some(Arc::new(move |_ctx| {
                let counter = counter.clone();
                Box::pin(async move {
                    *counter.lock() += 1;
                    Ok(vec![AnyModel::Chat(test_model("dyn", "fetched"))])
                })
            })),
            api: Some(ProviderApi::Single(pi_ai::api::registry::builtin_api("test-api"))),
            ..Default::default()
        })
        .unwrap()
    };
    let first = create_models(CreateModelsOptions { models_store: Some(store.clone()), ..Default::default() });
    first.set_provider(make());
    first.refresh(ModelsRefreshOptions::default()).await;
    assert!(first.get_model("dyn", "fetched").is_some());
    assert!(first.get_model("dyn", "base").is_some());
    assert!(store.read("dyn", None).await.unwrap().is_some());

    let second = create_models(CreateModelsOptions { models_store: Some(store.clone()), ..Default::default() });
    second.set_provider(make());
    second.refresh(ModelsRefreshOptions { allow_network: Some(false), ..Default::default() }).await;
    assert!(second.get_model("dyn", "fetched").is_some(), "restored without network");
    assert_eq!(*fetches.lock(), 1);
}

#[tokio::test]
async fn auth_resolution_order() {
    let credentials: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions { credentials: Some(credentials.clone()), ..Default::default() });
    let (oauth, _) = oauth_with(renewing());
    let (p1, _) = provider("p1", ProviderAuth { api_key: Some(Arc::new(EnvKey { value: Some("env-key".into()), login_key: None })), oauth: Some(oauth) });
    models.set_provider(p1);
    let model = AnyModel::Chat(test_model("p1", "model-a"));
    assert_eq!(models.get_auth_for_model(&model, Default::default()).await.unwrap().unwrap().auth.api_key.as_deref(), Some("env-key"));
    let explicit = AuthResolutionOverrides { api_key: Some("explicit-key".into()), ..Default::default() };
    assert_eq!(models.get_auth("p1", explicit).await.unwrap().unwrap().auth.api_key.as_deref(), Some("explicit-key"));

    store_credential(&credentials, "p1", oauth_credential("oauth-token", utils::now_ms() + 600_000)).await;
    let resolution = models.get_auth("p1", Default::default()).await.unwrap().unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("oauth-token"));
    assert_eq!(resolution.source.as_deref(), Some("OAuth"));

    store_credential(&credentials, "p1", Credential::ApiKey(ApiKeyCredential { key: Some("stored-key".into()), env: None })).await;
    let resolution = models.get_auth("p1", Default::default()).await.unwrap().unwrap();
    assert_eq!(resolution.auth.api_key.as_deref(), Some("stored-key"));
    assert_eq!(resolution.source.as_deref(), Some("stored"));
}

#[tokio::test]
async fn check_auth_get_available_login_logout() {
    let credentials: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions { credentials: Some(credentials.clone()), ..Default::default() });
    models.set_provider(provider("ambient", ProviderAuth { api_key: Some(Arc::new(EnvKey { value: Some("k".into()), login_key: None })), oauth: None }).0);
    models.set_provider(provider("missing", ProviderAuth { api_key: Some(Arc::new(EnvKey { value: None, login_key: Some("logged-in".into()) })), oauth: None }).0);
    let (oauth, refreshes) = oauth_with(renewing());
    models.set_provider(provider("oauth", ProviderAuth { api_key: None, oauth: Some(oauth) }).0);
    store_credential(&credentials, "oauth", oauth_credential("expired", 0)).await;

    let ambient = models.check_auth("ambient", None).await.unwrap().unwrap();
    assert_eq!((ambient.source.as_deref(), ambient.auth_type), (Some("env"), AuthType::ApiKey));
    assert!(models.check_auth("missing", None).await.unwrap().is_none());
    assert_eq!(models.check_auth("oauth", None).await.unwrap().unwrap().auth_type, AuthType::OAuth);
    assert_eq!(*refreshes.lock(), 0);
    let available: Vec<String> = models.get_available(None, None).await.unwrap().into_iter().map(|m| m.provider).collect();
    assert_eq!(available, vec!["ambient", "oauth"]);

    struct NoopInteraction;
    #[async_trait]
    impl AuthInteraction for NoopInteraction {
        async fn prompt(&self, _p: AuthPrompt) -> anyhow::Result<String> {
            Ok("unused".into())
        }
        fn notify(&self, _e: AuthEvent) {}
    }
    let credential = models.login("missing", AuthType::ApiKey, Arc::new(NoopInteraction), None, None).await.unwrap();
    assert_eq!(credential, Credential::ApiKey(ApiKeyCredential { key: Some("logged-in".into()), env: None }));
    assert_eq!(credentials.read("missing", None).await.unwrap(), Some(credential));
    models.logout("missing", None).await.unwrap();
    assert!(credentials.read("missing", None).await.unwrap().is_none());
    let unsupported = models.login("ambient", AuthType::OAuth, Arc::new(NoopInteraction), None, None).await.unwrap_err();
    assert_eq!(unsupported.to_string(), "ambient does not support oauth login");
}

#[tokio::test]
async fn oauth_refresh_rules() {
    let credentials: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    let models = create_models(CreateModelsOptions { credentials: Some(credentials.clone()), ..Default::default() });
    // stored oauth without a handler blocks ambient fallback
    models.set_provider(provider("plain", ProviderAuth { api_key: Some(Arc::new(EnvKey { value: Some("env".into()), login_key: None })), oauth: None }).0);
    store_credential(&credentials, "plain", oauth_credential("a", 0)).await;
    assert!(models.get_auth("plain", Default::default()).await.unwrap().is_none());

    // expired -> refreshed and persisted
    let (oauth, refreshes) = oauth_with(renewing());
    models.set_provider(provider("p1", ProviderAuth { api_key: None, oauth: Some(oauth) }).0);
    store_credential(&credentials, "p1", oauth_credential("old-token", 0)).await;
    assert_eq!(models.get_auth("p1", Default::default()).await.unwrap().unwrap().auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(credentials.read("p1", None).await.unwrap().unwrap().as_oauth().unwrap().access, "new-token");
    assert_eq!(*refreshes.lock(), 1);

    // less than five minutes remaining -> refresh
    store_credential(&credentials, "p1", oauth_credential("old-token", utils::now_ms() + 60_000)).await;
    assert_eq!(models.get_auth("p1", Default::default()).await.unwrap().unwrap().auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(*refreshes.lock(), 2);

    // caller's longer minimum validity
    store_credential(&credentials, "p1", oauth_credential("old-token", utils::now_ms() + 600_000)).await;
    let overrides = AuthResolutionOverrides { min_oauth_validity_ms: Some(30 * 60_000), ..Default::default() };
    assert_eq!(models.get_auth("p1", overrides).await.unwrap().unwrap().auth.api_key.as_deref(), Some("new-token"));
    assert_eq!(*refreshes.lock(), 3);

    // failure -> code oauth, credential preserved
    let (failing, _) = oauth_with(Arc::new(|_| anyhow::bail!("invalid_grant")));
    models.set_provider(provider("p2", ProviderAuth { api_key: None, oauth: Some(failing) }).0);
    store_credential(&credentials, "p2", oauth_credential("old", 0)).await;
    let error = models.get_auth("p2", Default::default()).await.unwrap_err();
    let models_error = error.downcast_ref::<ModelsError>().unwrap();
    assert_eq!(models_error.code, ModelsErrorCode::OAuth);
    assert_eq!(models_error.message, "OAuth refresh failed for p2: invalid_grant");
    assert_eq!(credentials.read("p2", None).await.unwrap().unwrap().as_oauth().unwrap().access, "old");
}

#[tokio::test]
async fn merges_auth_into_stream_options() {
    struct Resolved;
    #[async_trait]
    impl ApiKeyAuth for Resolved {
        fn name(&self) -> &str {
            "Test"
        }
        async fn resolve(&self, _i: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
            let mut headers = ProviderHeaders::new();
            headers.insert("Authorization".into(), Some("Bearer resolved-key".into()));
            headers.insert("x-a".into(), Some("auth".into()));
            headers.insert("x-b".into(), Some("auth".into()));
            Ok(Some(AuthResult {
                auth: ModelAuth { api_key: Some("resolved-key".into()), headers: Some(headers), base_url: Some("https://auth.test/v1".into()) },
                env: None,
                source: None,
            }))
        }
    }
    let models = create_models(CreateModelsOptions::default());
    let (p1, calls) = provider("p1", ProviderAuth { api_key: Some(Arc::new(Resolved)), oauth: None });
    models.set_provider(p1);
    let model = test_model("p1", "model-a");
    let mut headers = ProviderHeaders::new();
    headers.insert("authorization".into(), Some("Explicit token".into()));
    headers.insert("x-b".into(), Some("explicit".into()));
    let result = models
        .complete_simple(&model, &ctx(), StreamOptions { api_key: Some("explicit-key".into()), headers: Some(headers), ..Default::default() })
        .await;
    assert_eq!(result.stop_reason, StopReason::Stop);
    let (call_model, options) = calls.lock()[0].clone();
    assert_eq!(options.api_key.as_deref(), Some("explicit-key"));
    let h = options.headers.unwrap();
    assert_eq!(h.len(), 3);
    assert_eq!(h.get("authorization").cloned().flatten().as_deref(), Some("Explicit token"));
    assert_eq!(h.get("x-a").cloned().flatten().as_deref(), Some("auth"));
    assert_eq!(h.get("x-b").cloned().flatten().as_deref(), Some("explicit"));
    assert_eq!(call_model.base_url, "https://auth.test/v1");
    models.complete_simple(&model, &ctx(), StreamOptions::default()).await;
    assert_eq!(calls.lock()[1].1.api_key.as_deref(), Some("resolved-key"));
}

#[tokio::test]
async fn model_headers_and_transform() {
    let models = create_models(CreateModelsOptions::default());
    let (p1, calls) = provider("p1", ProviderAuth { api_key: Some(Arc::new(EnvKey { value: Some("key".into()), login_key: None })), oauth: None });
    models.set_provider(p1);
    let mut model = test_model("p1", "model-a");
    let mut model_headers = indexmap::IndexMap::new();
    model_headers.insert("x-model".to_string(), "model".to_string());
    model_headers.insert("x-shared".to_string(), "model".to_string());
    model.headers = Some(model_headers);
    assert!(models.get_auth("p1", Default::default()).await.unwrap().unwrap().auth.headers.is_none());
    let mut headers = ProviderHeaders::new();
    headers.insert("x-explicit".into(), Some("explicit".into()));
    headers.insert("X-Shared".into(), Some("explicit".into()));
    let transforms = Arc::new(Mutex::new(0));
    let counter = transforms.clone();
    let request = ModelsRequestOptions {
        options: StreamOptions { headers: Some(headers), ..Default::default() },
        transform_headers: Some(Arc::new(move |mut h: ProviderHeaders| {
            *counter.lock() += 1;
            assert_eq!(h.len(), 3);
            h.insert("x-transformed".into(), Some("yes".into()));
            Box::pin(async move { h })
        })),
    };
    models.complete_simple(&model, &ctx(), request).await;
    assert_eq!(*transforms.lock(), 1);
    let h = calls.lock()[0].1.headers.clone().unwrap();
    assert_eq!(h.keys().cloned().collect::<Vec<_>>(), vec!["x-model", "x-explicit", "X-Shared", "x-transformed"]);
}

#[tokio::test]
async fn unknown_provider_and_streaming() {
    let models = create_models(CreateModelsOptions::default());
    let result = models.complete_simple(&test_model("ghost", "model-a"), &ctx(), StreamOptions::default()).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error_message.unwrap().contains("Unknown provider: ghost"));
    models.set_provider(provider("p1", ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None }).0);
    let (events, message) = models.stream_simple(&test_model("p1", "model-a"), &ctx(), StreamOptions::default()).collect_all().await;
    assert_eq!(events.iter().map(|e| e.type_name()).collect::<Vec<_>>(), vec!["start", "done"]);
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn aborted_requests_fail_auth_setup_with_the_signal_reason() {
    // An agent aborted while its tools ran still makes one more request with the aborted signal.
    // `resolveProviderAuth` races its signal, which rejects with the signal's reason (the
    // DOMException "This operation was aborted"), and `lazyStream` turns the setup failure into
    // an error result before any provider request (not provider-retry's "Request aborted").
    let models = create_models(CreateModelsOptions::default());
    let (p1, calls) = provider("p1", ProviderAuth { api_key: Some(Arc::new(Ambient)), oauth: None });
    models.set_provider(p1);
    let model = test_model("p1", "model-a");
    let signal = CancellationToken::new();
    signal.cancel();
    for api_key in [None, Some("explicit-key".to_string())] {
        let options = StreamOptions { api_key, signal: Some(signal.clone()), ..Default::default() };
        let (events, message) = models.stream_simple(&model, &ctx(), options.clone()).collect_all().await;
        assert_eq!(events.iter().map(|e| e.type_name()).collect::<Vec<_>>(), vec!["error"]);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("This operation was aborted"));
        let message = models.complete(&model, &ctx(), options).await;
        assert_eq!(message.error_message.as_deref(), Some("This operation was aborted"));
    }
    assert!(calls.lock().is_empty());

    // Aborted while auth resolution is pending: the race rejects with the same reason.
    struct Pending;
    #[async_trait]
    impl ApiKeyAuth for Pending {
        fn name(&self) -> &str {
            "Pending"
        }
        async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
            std::future::pending().await
        }
    }
    let (p2, calls) = provider("p2", ProviderAuth { api_key: Some(Arc::new(Pending)), oauth: None });
    models.set_provider(p2);
    let signal = CancellationToken::new();
    let trigger = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let options = StreamOptions { signal: Some(signal), ..Default::default() };
    let message = models.complete_simple(&test_model("p2", "model-a"), &ctx(), options).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("This operation was aborted"));
    assert!(calls.lock().is_empty());
}
