//! Port of `packages/coding-agent/src/extensions/llama/provider.ts`.

use crate::extensions::llama::client::{
    LlamaClient, LlamaClientListOptions, LlamaClientPropsOptions, LlamaModelInfo, LlamaModelStatus, LlamaServerProps,
    llama_inference_url, normalize_llama_server_url,
};
use async_trait::async_trait;
use parking_lot::RwLock;
use pi_ai::api::llama_cpp_classify::LlamaCppClassifyApi;
use pi_ai::api::typesafe_system_one::TypesafeSystemOneApi;
use pi_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthCheck, AuthContext, AuthPrompt, AuthResult, AuthType,
    Credential, ModelAuth, ProviderAuth, ProviderAuthInteraction,
};
use pi_ai::models::{ModelsPublication, Provider, ProviderClassifier, RefreshModelsContext};
use pi_ai::models_store::ModelsStoreEntry;
use pi_ai::providers::compat_schema::{MaxTokensField, ModelCompat, ThinkingFormat};
use pi_ai::providers::model_schema::{ModelInputModality, ModelThinkingLevel, ThinkingLevelMap};
use pi_ai::types::{
    AnyModel, AssistantMessageEventStream, ClassifierContext, ClassifierModel, ClassifierResult, Model, ModelCost,
    StreamOptions, TranscriptContext,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";

pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LlamaClassifierApi {
    #[serde(rename = "llama-cpp-classify")]
    LlamaCppClassify,
    #[serde(rename = "typesafe-system-one")]
    TypesafeSystemOne,
}

impl LlamaClassifierApi {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LlamaCppClassify => "llama-cpp-classify",
            Self::TypesafeSystemOne => "typesafe-system-one",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaProviderControllerSetCatalogOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_autoload: Option<bool>,
}

pub trait LlamaProviderController: Send + Sync {
    fn provider(&self) -> Arc<dyn Provider>;
    fn set_provider(&self, value: Arc<dyn Provider>);
    fn set_catalog(
        &self,
        models: &[LlamaModelInfo],
        server_url: &str,
        options: Option<&LlamaProviderControllerSetCatalogOptions>,
    );
}

fn credential_server_url(credential: Option<&ApiKeyCredential>) -> Option<String> {
    let value = credential?.env.as_ref()?.get("LLAMA_BASE_URL")?;
    if value.trim().is_empty() {
        return None;
    }
    normalize_llama_server_url(value).ok()
}

async fn resolve_server_url(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<String> {
    let configured = match credential_server_url(credential) {
        Some(url) => Some(url),
        None => ctx.env("LLAMA_BASE_URL").await.map(|v| v.trim().to_string()),
    };
    configured.filter(|url| !url.is_empty()).and_then(|url| normalize_llama_server_url(&url).ok())
}

fn model_is_selectable(model: &LlamaModelInfo, router_autoload: bool) -> bool {
    match model.status.value {
        LlamaModelStatus::Loaded => true,
        // llama.cpp reports idle-slept models as "sleeping"; requests wake them automatically.
        LlamaModelStatus::Sleeping => true,
        // Unloaded presets are routable only when llama.cpp router autoload can load them on first use.
        LlamaModelStatus::Unloaded => {
            router_autoload && model.status.failed != Some(true) && model.source.as_deref() == Some("preset")
        }
        _ => false,
    }
}

async fn router_autoload_enabled(client: &LlamaClient, catalog: &[LlamaModelInfo], signal: &CancellationToken) -> bool {
    if !catalog
        .iter()
        .any(|model| model.status.value == LlamaModelStatus::Unloaded && model.source.as_deref() == Some("preset"))
    {
        return false;
    }
    match client.props(Some(&LlamaClientPropsOptions { model: None, signal: Some(signal.clone()) })).await {
        Ok(props) => props.models_autoload == Some(true),
        Err(_) => false,
    }
}

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn configured_context_window(model: &LlamaModelInfo) -> Option<u64> {
    let args = model.status.args.as_deref().unwrap_or_default();
    for index in 0..args.len().saturating_sub(1) {
        let flag = args[index].as_str();
        if flag != "--ctx-size" && flag != "-c" && flag != "-ctx" {
            continue;
        }
        let text = args[index + 1].trim();
        let value: f64 = if text.is_empty() { 0.0 } else { text.parse().unwrap_or(f64::NAN) };
        if value.fract() == 0.0 && value > 0.0 && value <= MAX_SAFE_INTEGER {
            return Some(value as u64);
        }
    }
    None
}

fn context_window_of(model: &LlamaModelInfo, cached: Option<u64>) -> u64 {
    let meta = model.meta.as_ref();
    if let Some(runtime) = meta.and_then(|m| m.n_ctx).filter(|n| *n > 0.0) {
        return runtime as u64;
    }
    if let Some(configured) = configured_context_window(model) {
        return configured;
    }
    if let Some(cached) = cached.filter(|c| *c > 0) {
        return cached;
    }
    meta.and_then(|m| m.n_ctx_train).filter(|n| *n > 0.0).map(|n| n as u64).unwrap_or(128_000)
}

fn has_output_modality(model: &LlamaModelInfo, modality: &str) -> bool {
    model
        .architecture
        .as_ref()
        .and_then(|a| a.output_modalities.as_ref())
        .is_some_and(|m| m.iter().any(|v| v == modality))
}

/// Whether llama.cpp reports a native decision model. Since llama.cpp 0.6.0, `GET /models` lists
/// `decisions` in `architecture.output_modalities` for these models, including unloaded and sleeping
/// ones. Older servers report `["text"]` or omit `architecture`, so their models are treated as chat
/// models.
fn is_decision_model(model: &LlamaModelInfo) -> bool {
    has_output_modality(model, "decisions")
}

/// Decision-only models cannot generate text and are not listed for chat.
fn is_chat_model(model: &LlamaModelInfo) -> bool {
    !is_decision_model(model) || has_output_modality(model, "text")
}

fn zero_cost() -> ModelCost {
    ModelCost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0, tiers: None }
}

/// A llama.cpp model used as a classifier. Decision models answer natively through llama.cpp's
/// System One endpoint (`/v1/systemone`). Chat models fall back to `llama-cpp-classify`, which reads
/// answers from next-token label probabilities.
fn to_pi_classifier_model(model: &LlamaModelInfo, server_url: &str, cached: Option<u64>) -> ClassifierModel {
    let decision = is_decision_model(model);
    ClassifierModel {
        id: model.id.clone(),
        name: model.id.clone(),
        api: if decision { LlamaClassifierApi::TypesafeSystemOne } else { LlamaClassifierApi::LlamaCppClassify }
            .as_str()
            .to_string(),
        provider: LLAMA_PROVIDER_ID.to_string(),
        base_url: if decision { llama_inference_url(server_url) } else { server_url.to_string() },
        input: vec![ModelInputModality::Text],
        cost: zero_cost(),
        context_window: context_window_of(model, cached),
        model_type: "classifier".to_string(),
        ..Default::default()
    }
}

fn is_llama_classifier_model(model: &AnyModel) -> Option<ClassifierModel> {
    match model {
        AnyModel::Classifier(c) if c.api == "llama-cpp-classify" || c.api == "typesafe-system-one" => Some(c.clone()),
        _ => None,
    }
}

fn to_pi_model(model: &LlamaModelInfo, server_url: &str, props: Option<&LlamaServerProps>, cached: Option<u64>) -> Model {
    let context_window = context_window_of(model, cached);
    let reasoning = props.and_then(|p| p.chat_template.as_deref()).is_some_and(|t| t.contains("enable_thinking"));
    let thinking_level_map: Option<ThinkingLevelMap> = reasoning.then(|| {
        [
            (ModelThinkingLevel::Off, Some("off".to_string())),
            (ModelThinkingLevel::Minimal, None),
            (ModelThinkingLevel::Low, None),
            (ModelThinkingLevel::Medium, Some("medium".to_string())),
            (ModelThinkingLevel::High, None),
            (ModelThinkingLevel::Xhigh, None),
        ]
        .into_iter()
        .collect()
    });
    let image_input = model
        .architecture
        .as_ref()
        .and_then(|a| a.input_modalities.as_ref())
        .is_some_and(|m| m.iter().any(|v| v == "image"));
    Model {
        id: model.id.clone(),
        name: model.id.clone(),
        api: "openai-completions".to_string(),
        provider: LLAMA_PROVIDER_ID.to_string(),
        base_url: llama_inference_url(server_url),
        reasoning,
        thinking_level_map,
        input: if image_input {
            vec![ModelInputModality::Text, ModelInputModality::Image]
        } else {
            vec![ModelInputModality::Text]
        },
        cost: zero_cost(),
        context_window,
        max_tokens: context_window,
        compat: Some(Box::new(ModelCompat {
            supports_store: Some(false),
            supports_developer_role: Some(false),
            supports_reasoning_effort: Some(false),
            supports_usage_in_streaming: Some(true),
            supports_strict_mode: Some(false),
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            thinking_format: reasoning.then_some(ThinkingFormat::QwenChatTemplate),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[derive(Default)]
struct Catalog {
    models: Vec<Model>,
    classifiers: Vec<ClassifierModel>,
}

/// The llama.cpp server's api-key auth: the server URL plus an optional key.
struct LlamaApiKeyAuth;

#[async_trait]
impl ApiKeyAuth for LlamaApiKeyAuth {
    fn name(&self) -> &str {
        "llama.cpp server"
    }

    fn supports_login(&self) -> bool {
        true
    }

    async fn login(&self, interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        let env_url = std::env::var("LLAMA_BASE_URL").ok();
        let mut prompt = AuthPrompt::text("llama.cpp server URL");
        prompt.placeholder = Some(env_url.clone().unwrap_or_else(|| DEFAULT_LLAMA_SERVER_URL.to_string()));
        let entered_url = interaction.interaction.prompt(prompt).await?;
        let entered = entered_url.trim();
        let server_url = normalize_llama_server_url(if !entered.is_empty() {
            entered
        } else {
            env_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(DEFAULT_LLAMA_SERVER_URL)
        })?;
        let api_key = interaction.interaction.prompt(AuthPrompt::secret("API key (optional)")).await?.trim().to_string();
        let key = (!api_key.is_empty()).then_some(api_key);
        LlamaClient::try_new(&server_url, key.as_deref())?
            .list(Some(&LlamaClientListOptions { reload: None, signal: Some(interaction.signal.clone()) }))
            .await?;
        Ok(ApiKeyCredential { key, env: Some(HashMap::from([("LLAMA_BASE_URL".to_string(), server_url)])) })
    }

    fn supports_check(&self) -> bool {
        true
    }

    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        let server_url = resolve_server_url(input.ctx.as_ref(), input.credential.as_ref()).await;
        Ok(server_url.map(|_| AuthCheck {
            source: Some(if input.credential.is_some() { "stored credential" } else { "LLAMA_BASE_URL" }.to_string()),
            auth_type: AuthType::ApiKey,
        }))
    }

    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        let Some(server_url) = resolve_server_url(input.ctx.as_ref(), input.credential.as_ref()).await else {
            return Ok(None);
        };
        let api_key = match input.credential.as_ref().and_then(|c| c.key.clone()) {
            Some(key) => key,
            None => input.ctx.env("LLAMA_API_KEY").await.unwrap_or_else(|| "local".to_string()),
        };
        let mut env = input.credential.as_ref().and_then(|c| c.env.clone()).unwrap_or_default();
        env.insert("LLAMA_BASE_URL".to_string(), server_url.clone());
        Ok(Some(AuthResult {
            auth: ModelAuth { api_key: Some(api_key), headers: None, base_url: Some(llama_inference_url(&server_url)) },
            env: Some(env),
            source: Some(if input.credential.is_some() { "stored credential" } else { "LLAMA_BASE_URL" }.to_string()),
        }))
    }
}

struct LlamaProvider {
    base_url: String,
    auth: ProviderAuth,
    catalog: Arc<RwLock<Catalog>>,
}

#[async_trait]
impl Provider for LlamaProvider {
    fn id(&self) -> &str {
        LLAMA_PROVIDER_ID
    }

    fn name(&self) -> &str {
        "llama.cpp"
    }

    fn base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.catalog.read().models.clone()
    }

    fn get_all_models(&self) -> Vec<AnyModel> {
        let catalog = self.catalog.read();
        catalog
            .models
            .iter()
            .cloned()
            .map(AnyModel::Chat)
            .chain(catalog.classifiers.iter().cloned().map(AnyModel::Classifier))
            .collect()
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        let mut cached_context_windows: HashMap<String, u64> = HashMap::new();
        if let Some(stored) = &context.stored {
            let stored: Vec<&AnyModel> = stored.models.iter().filter(|m| m.provider() == LLAMA_PROVIDER_ID).collect();
            let restored: Vec<Model> = stored
                .iter()
                .filter_map(|m| match m {
                    AnyModel::Chat(model) if model.api == "openai-completions" => Some(model.clone()),
                    _ => None,
                })
                .collect();
            let restored_classifiers: Vec<ClassifierModel> =
                stored.iter().filter_map(|m| is_llama_classifier_model(m)).collect();
            for model in &restored {
                cached_context_windows.insert(model.id.clone(), model.context_window);
            }
            for model in &restored_classifiers {
                cached_context_windows.insert(model.id.clone(), model.context_window);
            }
            let catalog = self.catalog.clone();
            let published = (context.publish)(ModelsPublication {
                persist: None,
                update: Some(Box::new(move || {
                    *catalog.write() = Catalog { models: restored, classifiers: restored_classifiers };
                })),
            })
            .await?;
            if !published {
                return Ok(());
            }
        }

        if !context.allow_network || context.signal.is_cancelled() {
            return Ok(());
        }
        let Some(Credential::ApiKey(credential)) = &context.credential else { return Ok(()) };
        let Some(server_url) = credential_server_url(Some(credential)) else { return Ok(()) };
        let client = LlamaClient::try_new(&server_url, credential.key.as_deref())?;
        let signal = context.signal.clone();
        let catalog = client.list(Some(&LlamaClientListOptions { reload: None, signal: Some(signal.clone()) })).await?;
        if signal.is_cancelled() {
            return Ok(());
        }
        let router_autoload = router_autoload_enabled(&client, &catalog, &signal).await;
        if signal.is_cancelled() {
            return Ok(());
        }
        let selectable: Vec<&LlamaModelInfo> =
            catalog.iter().filter(|model| model_is_selectable(model, router_autoload)).collect();
        // Only loaded models expose their chat template without side effects. Unloaded autoload
        // presets would need to be loaded, while querying sleeping models may wake them. Those models
        // remain without thinking support until they are loaded and a later catalog refresh discovers
        // it. Decision models need no template, and llama.cpp reports them in the catalog regardless
        // of their status.
        let refreshed = futures::future::try_join_all(selectable.iter().filter(|m| is_chat_model(m)).map(|model| {
            let cached = cached_context_windows.get(&model.id).copied();
            let (client, server_url, signal) = (&client, &server_url, &signal);
            async move {
                if model.status.value != LlamaModelStatus::Loaded {
                    return anyhow::Ok(to_pi_model(model, server_url, None, cached));
                }
                let props = client
                    .props(Some(&LlamaClientPropsOptions { model: Some(model.id.clone()), signal: Some(signal.clone()) }))
                    .await?;
                Ok(to_pi_model(model, server_url, Some(&props), cached))
            }
        }))
        .await?;
        let refreshed_classifiers: Vec<ClassifierModel> = selectable
            .iter()
            .map(|model| to_pi_classifier_model(model, &server_url, cached_context_windows.get(&model.id).copied()))
            .collect();
        if signal.is_cancelled() {
            return Ok(());
        }
        let persisted: Vec<AnyModel> = refreshed
            .iter()
            .cloned()
            .map(AnyModel::Chat)
            .chain(refreshed_classifiers.iter().cloned().map(AnyModel::Classifier))
            .collect();
        let catalog_state = self.catalog.clone();
        (context.publish)(ModelsPublication {
            persist: Some(Some(ModelsStoreEntry {
                models: persisted,
                checked_at: Some(chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            })),
            update: Some(Box::new(move || {
                *catalog_state.write() = Catalog { models: refreshed, classifiers: refreshed_classifiers };
            })),
        })
        .await?;
        Ok(())
    }

    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        pi_ai::api::openai_completions::stream(model, context, options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        pi_ai::api::openai_completions::stream_simple(model, context, options)
    }

    fn supports_classify(&self) -> bool {
        true
    }

    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        if model.api == "typesafe-system-one" {
            TypesafeSystemOneApi.classify(model, context, options).await
        } else {
            LlamaCppClassifyApi.classify(model, context, options).await
        }
    }
}

struct Controller {
    provider: RwLock<Arc<dyn Provider>>,
    catalog: Arc<RwLock<Catalog>>,
}

impl LlamaProviderController for Controller {
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.read().clone()
    }

    fn set_provider(&self, value: Arc<dyn Provider>) {
        *self.provider.write() = value;
    }

    fn set_catalog(
        &self,
        models: &[LlamaModelInfo],
        server_url: &str,
        options: Option<&LlamaProviderControllerSetCatalogOptions>,
    ) {
        let router_autoload = options.and_then(|o| o.router_autoload) == Some(true);
        let selectable: Vec<&LlamaModelInfo> =
            models.iter().filter(|model| model_is_selectable(model, router_autoload)).collect();
        *self.catalog.write() = Catalog {
            models: selectable.iter().filter(|m| is_chat_model(m)).map(|m| to_pi_model(m, server_url, None, None)).collect(),
            classifiers: selectable.iter().map(|m| to_pi_classifier_model(m, server_url, None)).collect(),
        };
    }
}

pub fn create_llama_provider() -> Arc<dyn LlamaProviderController> {
    let catalog = Arc::new(RwLock::new(Catalog::default()));
    let provider: Arc<dyn Provider> = Arc::new(LlamaProvider {
        base_url: llama_inference_url(DEFAULT_LLAMA_SERVER_URL),
        auth: ProviderAuth { api_key: Some(Arc::new(LlamaApiKeyAuth)), oauth: None },
        catalog: catalog.clone(),
    });
    Arc::new(Controller { provider: RwLock::new(provider), catalog })
}
