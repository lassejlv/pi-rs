//! Port of identity.test.ts: client identity (`User-Agent`, provider attribution) and how
//! requests apply it.
//!
//! TS captures headers with a `fetch` mock; here requests go to a local server. Targets are
//! detected by provider ID, or for the OpenCode host through a client that resolves
//! `opencode.ai` to the local server.

use std::sync::Arc;

use pi_ai::api::openai_completions;
use pi_ai::auth::{ApiKeyAuth, ApiKeyResolveInput, AuthResult, ModelAuth, ProviderAuth};
use pi_ai::utils::identity::{
    ClientAttribution, ClientIdentity, ClientIdentityTarget, UserAgentMode, UserAgentNamed, format_user_agent,
    get_attribution_headers, get_identity_headers, merge_client_identity, pi_attribution,
};
use pi_ai::utils::version::PI_AI_VERSION;
use pi_ai::*;
use serde_json::{Value, json};

use crate::mock_http::{MockServer, Request, Response};
use crate::support::*;

fn acme() -> ClientAttribution {
    ClientAttribution { name: "acme".into(), url: "https://acme.example".into(), categories: None }
}

fn attributed(attribution: ClientAttribution) -> ClientIdentity {
    ClientIdentity { attribution: Some(Some(attribution)), ..Default::default() }
}

fn target<'a>(provider: &'a str, base_url: &'a str) -> ClientIdentityTarget<'a> {
    ClientIdentityTarget { provider, base_url }
}

fn headers(pairs: &[(&str, &str)]) -> indexmap::IndexMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn short() -> Option<UserAgentMode> {
    Some(UserAgentMode::Named(UserAgentNamed::Short))
}

/// `pi/<version> (<platform>; <runtime>; <arch>)`; the runtime is `rust/<version>` here.
fn detailed_suffix(user_agent: &str, version: &str) -> bool {
    user_agent.starts_with(&format!("pi/{version} (")) && user_agent.ends_with(')') && user_agent.contains("; rust/")
}

#[test]
fn keeps_the_version_constant_in_sync_with_the_package() {
    assert_eq!(PI_AI_VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn formats_user_agents() {
    assert!(detailed_suffix(&format_user_agent(None), PI_AI_VERSION), "{}", format_user_agent(None));
    let versioned = ClientIdentity { version: Some("9.9.9".into()), ..Default::default() };
    assert!(detailed_suffix(&format_user_agent(Some(&versioned)), "9.9.9"));
    // Only the version differs from the default.
    assert_eq!(format_user_agent(Some(&versioned)).replacen("9.9.9", PI_AI_VERSION, 1), format_user_agent(None));
    let short_versioned = ClientIdentity { user_agent: short(), version: Some("9.9.9".into()), ..Default::default() };
    assert_eq!(format_user_agent(Some(&short_versioned)), "pi");
    let custom = ClientIdentity { user_agent: Some(UserAgentMode::Custom { custom: "acme/1.0".into() }), ..Default::default() };
    assert_eq!(format_user_agent(Some(&custom)), "acme/1.0");
}

#[test]
fn merges_identities_field_by_field() {
    assert_eq!(merge_client_identity(&[None, None]), None);
    let base = ClientIdentity { attribution: Some(Some(pi_attribution())), user_agent: short(), ..Default::default() };
    let over = ClientIdentity { attribution: None, version: Some("1.0.0".into()), ..Default::default() };
    assert_eq!(
        merge_client_identity(&[Some(&base), Some(&over)]),
        Some(ClientIdentity { attribution: Some(Some(pi_attribution())), user_agent: short(), version: Some("1.0.0".into()) })
    );
    let off = ClientIdentity { attribution: Some(None), ..Default::default() };
    assert_eq!(
        merge_client_identity(&[Some(&attributed(pi_attribution())), Some(&off)]),
        Some(ClientIdentity { attribution: Some(None), ..Default::default() })
    );
}

#[test]
fn sends_no_attribution_unless_enabled() {
    let open_router = target("openrouter", "https://openrouter.ai/api/v1");
    assert!(get_attribution_headers(open_router, None).is_empty());
    let off = ClientIdentity { attribution: Some(None), ..Default::default() };
    assert!(get_attribution_headers(open_router, Some(&off)).is_empty());
    assert_eq!(get_identity_headers(open_router, None), headers(&[("User-Agent", &format_user_agent(None))]));
}

#[test]
fn attributes_open_router_requests() {
    let open_router = target("openrouter", "https://openrouter.ai/api/v1");
    assert_eq!(
        get_attribution_headers(open_router, Some(&attributed(pi_attribution()))),
        headers(&[("HTTP-Referer", "https://pi.dev"), ("X-OpenRouter-Title", "pi"), ("X-OpenRouter-Categories", "cli-agent")])
    );
    let categories = ClientAttribution { categories: Some(vec!["cli-agent".into(), "cloud-agent".into()]), ..acme() };
    assert_eq!(
        get_attribution_headers(open_router, Some(&attributed(categories))),
        headers(&[
            ("HTTP-Referer", "https://acme.example"),
            ("X-OpenRouter-Title", "acme"),
            ("X-OpenRouter-Categories", "cli-agent,cloud-agent")
        ])
    );
    let no_categories = ClientAttribution { categories: Some(vec![]), ..acme() };
    assert_eq!(
        get_attribution_headers(open_router, Some(&attributed(no_categories))),
        headers(&[("HTTP-Referer", "https://acme.example"), ("X-OpenRouter-Title", "acme")])
    );
    // Custom providers routed through OpenRouter, including legacy base URLs that are not valid URLs.
    let legacy = get_attribution_headers(target("custom", "not-a-url-openrouter.ai"), Some(&attributed(pi_attribution())));
    assert_eq!(legacy.get("X-OpenRouter-Title").map(String::as_str), Some("pi"));
}

#[test]
fn attributes_nvidia_nim_with_a_capitalized_name_and_opencode_with_the_name() {
    let pi = attributed(pi_attribution());
    assert_eq!(
        get_attribution_headers(target("nvidia", "https://example.test/v1"), Some(&pi)),
        headers(&[("X-BILLING-INVOKE-ORIGIN", "Pi")])
    );
    assert_eq!(
        get_attribution_headers(target("custom-nim", "https://integrate.api.nvidia.com/v1"), Some(&attributed(acme()))),
        headers(&[("X-BILLING-INVOKE-ORIGIN", "Acme")])
    );
    assert_eq!(
        get_attribution_headers(target("opencode", "https://opencode.ai/zen/v1"), Some(&pi)),
        headers(&[("x-opencode-client", "pi")])
    );
    assert_eq!(
        get_attribution_headers(target("custom", "https://opencode.ai/zen/v1"), Some(&attributed(acme()))),
        headers(&[("x-opencode-client", "acme")])
    );
}

#[test]
fn does_not_attribute_nvidia_models_routed_through_gateways_to_nvidia() {
    let pi = attributed(pi_attribution());
    assert!(!get_attribution_headers(target("openrouter", "https://openrouter.ai/api/v1"), Some(&pi)).contains_key("X-BILLING-INVOKE-ORIGIN"));
    assert!(get_attribution_headers(target("vercel-ai-gateway", "https://ai-gateway.vercel.sh/v1"), Some(&pi)).is_empty());
}

// ---------------------------------------------------------------------------
// client identity in requests
// ---------------------------------------------------------------------------

fn completions_model(provider: &str, base_url: &str, model_headers: Option<Value>) -> Model {
    let mut value = json!({
        "id": "test-model", "name": "Test Model", "api": "openai-completions", "provider": provider, "baseUrl": base_url,
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128000, "maxTokens": 4096
    });
    if let Some(model_headers) = model_headers {
        value["headers"] = model_headers;
    }
    model(value)
}

async fn completions_server() -> MockServer {
    let body = openai_sse(&[
        json!({"id": "chatcmpl-identity", "choices": [{"delta": {"content": "ok"}, "finish_reason": null, "index": 0}]}),
        json!({"id": "chatcmpl-identity", "choices": [{"delta": {}, "finish_reason": "stop", "index": 0}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
    ]);
    MockServer::start(move |_, _| Response::sse(&body)).await
}

fn hello() -> TranscriptContext {
    normalize_context(&user_ctx("hello"))
}

fn last_request(server: &MockServer) -> Request {
    server.requests().last().cloned().expect("no request was sent")
}

struct KeyAuth;

#[async_trait::async_trait]
impl ApiKeyAuth for KeyAuth {
    fn name(&self) -> &str {
        "Test"
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        Ok(Some(AuthResult {
            auth: ModelAuth { api_key: Some("key".into()), ..Default::default() },
            env: None,
            source: None,
        }))
    }
}

fn completions_provider(model: &Model) -> Arc<dyn Provider> {
    create_provider(CreateProviderOptions {
        id: model.provider.clone(),
        auth: ProviderAuth { api_key: Some(Arc::new(KeyAuth)), oauth: None },
        models: vec![AnyModel::Chat(model.clone())],
        api: Some(ProviderApi::Single(Arc::new(openai_completions::OpenAICompletionsApi))),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn applies_the_identity_below_model_and_request_headers() {
    let server = completions_server().await;
    let model = completions_model("openrouter", &server.url, Some(json!({"X-OpenRouter-Title": "model-title"})));
    let mut request_headers = ProviderHeaders::new();
    request_headers.insert("HTTP-Referer".into(), None);
    let identity = ClientIdentity { attribution: Some(Some(pi_attribution())), user_agent: short(), ..Default::default() };
    let options = StreamOptions {
        api_key: Some("key".into()),
        identity: Some(identity),
        headers: Some(request_headers),
        ..Default::default()
    };
    openai_completions::stream(&model, &hello(), options).result().await;

    let request = last_request(&server);
    assert_eq!(request.header("user-agent"), Some("pi"));
    assert_eq!(request.header("x-openrouter-title"), Some("model-title"));
    assert_eq!(request.header("http-referer"), None);
    assert_eq!(request.header("x-openrouter-categories"), Some("cli-agent"));
}

#[tokio::test]
async fn keeps_impersonating_model_headers_over_the_identity_user_agent() {
    let server = completions_server().await;
    let model = completions_model("github-copilot", &server.url, Some(json!({"User-Agent": "GitHubCopilotChat/0.35.0"})));
    let identity = ClientIdentity { user_agent: Some(UserAgentMode::Custom { custom: "acme".into() }), ..Default::default() };
    let options = StreamOptions { api_key: Some("key".into()), identity: Some(identity), ..Default::default() };
    openai_completions::stream(&model, &hello(), options).result().await;

    assert_eq!(last_request(&server).header("user-agent"), Some("GitHubCopilotChat/0.35.0"));
}

#[tokio::test]
async fn merges_request_identity_over_the_models_and_wrapper_identities() {
    let server = completions_server().await;
    let model = completions_model("openrouter", &server.url, None);
    let attribution = |name: &str| ClientAttribution { name: name.into(), url: "https://acme.example".into(), categories: None };
    let models = create_models(CreateModelsOptions {
        identity: Some(ClientIdentity { attribution: Some(Some(attribution("models"))), user_agent: short(), ..Default::default() }),
        ..Default::default()
    });
    models.set_provider(completions_provider(&model));
    let send = |target: Models, identity: Option<ClientIdentity>| {
        let model = model.clone();
        async move {
            let context = user_ctx("hello");
            target.stream_simple(&model, &context, StreamOptions { identity, ..Default::default() }).result().await
        }
    };

    send(models.clone(), None).await;
    let from_models = last_request(&server);
    assert_eq!(from_models.header("user-agent"), Some("pi"));
    assert_eq!(from_models.header("x-openrouter-title"), Some("models"));

    let wrapped = models.with_client_identity(attributed(attribution("wrapper")));
    send(wrapped.clone(), None).await;
    let from_wrapper = last_request(&server);
    assert_eq!(from_wrapper.header("user-agent"), Some("pi"));
    assert_eq!(from_wrapper.header("x-openrouter-title"), Some("wrapper"));
    assert_eq!(wrapped.get_model("openrouter", "test-model"), Some(model.clone()));
    // The wrapper stays mutable and shares providers.
    wrapped.delete_provider("openrouter");
    assert!(models.get_provider("openrouter").is_none());
    models.set_provider(completions_provider(&model));

    send(wrapped, Some(ClientIdentity { attribution: Some(None), ..Default::default() })).await;
    let from_request = last_request(&server);
    assert_eq!(from_request.header("x-openrouter-title"), None);
    assert_eq!(from_request.header("user-agent"), Some("pi"));
}

#[tokio::test]
async fn sends_the_opencode_session_header_to_custom_providers_pointing_at_opencode() {
    let server = completions_server().await;
    let port = server.url.rsplit(':').next().unwrap().to_string();
    // `https://opencode.ai/zen/v1` in TS; the host must stay `opencode.ai` for target detection.
    let model = completions_model("custom-opencode", &format!("http://opencode.ai:{port}/zen/v1"), None);
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("opencode.ai", "127.0.0.1:0".parse().unwrap())
        .build()
        .unwrap();
    let models = create_models(CreateModelsOptions::default());
    models.set_provider(completions_provider(&model));
    let send = |request_headers: Option<ProviderHeaders>| {
        let models = models.clone();
        let model = model.clone();
        let client = client.clone();
        async move {
            let options = StreamOptions {
                http_client: Some(client),
                session_id: Some("session-1".into()),
                headers: request_headers,
                ..Default::default()
            };
            let message = models.stream_simple(&model, &user_ctx("hello"), options).result().await;
            assert_ne!(message.stop_reason, StopReason::Error, "{:?}", message.error_message);
        }
    };

    send(None).await;
    assert_eq!(last_request(&server).header("x-opencode-session"), Some("session-1"));
    let mut configured = ProviderHeaders::new();
    configured.insert("X-OpenCode-Session".into(), Some("configured".into()));
    send(Some(configured)).await;
    assert_eq!(last_request(&server).header("x-opencode-session"), Some("configured"));
}
