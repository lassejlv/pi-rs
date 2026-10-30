//! Port of `packages/coding-agent/src/core/provider-composer.ts`.
//!
//! Composes a provider from three layers without reading credentials: the built-in (or native
//! extension) provider, the models.json entry, and an extension `registerProvider()` config.

use crate::core::extensions::types::ProviderOAuthConfig;
use crate::core::model_config::{
    ModelConfig, ModelsJsonModel, ModelsJsonModelOverride, ModelsJsonProvider,
};
use crate::core::resolve_config_value::{
    clear_config_value_cache, get_config_value_env_var_names, is_command_config_value,
    is_config_value_configured, resolve_config_value_or_throw,
};
use async_trait::async_trait;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use pi_ai::api::lazy::error_stream;
use pi_ai::auth::extension_oauth_types::{
    OAuthAuthInfo, OAuthDeviceCodeInfo, OAuthLoginCallbacks, OAuthPrompt, OAuthSelectPrompt,
};
use pi_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthCheck, AuthContext, AuthEvent,
    AuthPrompt, AuthPromptKind, AuthResult, AuthSelectOption, AuthType, Credential, LoginOptions,
    ModelAuth, OAuthAuth, OAuthCredentials, ProviderAuth, ProviderAuthInteraction,
};
use pi_ai::compat::get_api_provider;
use pi_ai::models::{ModelsPublication, Provider, RefreshModelsContext};
use pi_ai::providers::model_schema::{ModelCost, ModelInputLimits, ModelInputModality};
use pi_ai::types::{
    AnyModel, AssistantImages, AssistantMessageEventStream, ClassifierContext, ClassifierModel,
    ClassifierResult, DeferredHandle, ImageModel, ImagesContext, Model, ModelCompat, ModelType,
    ProviderHeaders, StreamOptions, TranscriptContext,
};
use pi_ai::utils::model_operations::{classifier_error_result, image_error_result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Legacy extension OAuth config (`ProviderConfig.oauth`).
pub type ExtensionOAuthConfig = ProviderOAuthConfig;

pub use crate::core::extensions::types::{
    ProviderChatModelConfig, ProviderClassifierModelConfig, ProviderImageModelConfig,
    ProviderModelConfig,
};

/// Input type for the extension registerProvider API (the extension `ProviderConfig`).
pub type ProviderConfigInput = crate::core::extensions::types::ProviderConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthStatusSource {
    #[serde(rename = "stored")]
    Stored,
    #[serde(rename = "runtime")]
    Runtime,
    #[serde(rename = "environment")]
    Environment,
    #[serde(rename = "fallback")]
    Fallback,
    #[serde(rename = "models_json_key")]
    ModelsJsonKey,
    #[serde(rename = "models_json_command")]
    ModelsJsonCommand,
}

impl AuthStatusSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Runtime => "runtime",
            Self::Environment => "environment",
            Self::Fallback => "fallback",
            Self::ModelsJsonKey => "models_json_key",
            Self::ModelsJsonCommand => "models_json_command",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AuthStatusSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl AuthStatus {
    pub fn unconfigured() -> Self {
        Self { configured: false, source: None, label: None }
    }
    pub fn configured(source: AuthStatusSource, label: Option<String>) -> Self {
        Self { configured: true, source: Some(source), label }
    }
}

/// Clear the cache of command-backed API key values.
pub fn clear_api_key_cache() {
    clear_config_value_cache();
}

type Headers = IndexMap<String, String>;

fn env_to_index_map(env: &pi_ai::types::ProviderEnv) -> IndexMap<String, String> {
    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn to_provider_headers(headers: &Headers) -> ProviderHeaders {
    headers.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect()
}

/// `resolveHeadersOrThrow`: resolve every header value like an API key, failing on the first
/// value that cannot be resolved.
fn resolve_headers_or_throw(
    headers: Option<&Headers>,
    description: &str,
    env: Option<&IndexMap<String, String>>,
) -> anyhow::Result<Option<Headers>> {
    let Some(headers) = headers else { return Ok(None) };
    let mut resolved = Headers::new();
    for (key, value) in headers {
        let value = resolve_config_value_or_throw(
            value,
            &format!("{description} header \"{key}\""),
            env.cloned(),
        )?;
        resolved.insert(key.clone(), value);
    }
    Ok((!resolved.is_empty()).then_some(resolved))
}

fn get_all_provider_models(provider: Option<&Arc<dyn Provider>>) -> Vec<AnyModel> {
    provider.map(|p| p.get_all_models()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Merging helpers
// ---------------------------------------------------------------------------

const NESTED_COMPAT_KEYS: [&str; 4] = [
    "openRouterRouting",
    "vercelGatewayRouting",
    "chatTemplateKwargs",
    "chatTemplateArgs",
];

/// `mergeCompat` on JSON objects: shallow spread plus one-level merge of the nested settings.
fn merge_compat_maps(base: Option<Map<String, Value>>, over: Option<&Map<String, Value>>) -> Option<Map<String, Value>> {
    let Some(over) = over else { return base };
    let mut merged = base.clone().unwrap_or_default();
    for (key, value) in over {
        merged.insert(key.clone(), value.clone());
    }
    for key in NESTED_COMPAT_KEYS {
        let base_value = base.as_ref().and_then(|b| b.get(key)).filter(|v| v.is_object());
        let over_value = over.get(key).filter(|v| v.is_object());
        if base_value.is_none() && over_value.is_none() {
            continue;
        }
        let mut nested = Map::new();
        for source in [base_value, over_value].into_iter().flatten() {
            if let Value::Object(entries) = source {
                for (k, v) in entries {
                    nested.insert(k.clone(), v.clone());
                }
            }
        }
        merged.insert(key.to_string(), Value::Object(nested));
    }
    Some(merged)
}

fn compat_to_map(compat: Option<&ModelCompat>) -> Option<Map<String, Value>> {
    compat.and_then(|c| match serde_json::to_value(c) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    })
}

fn map_to_compat(map: Option<Map<String, Value>>) -> Option<Box<ModelCompat>> {
    map.and_then(|m| serde_json::from_value(Value::Object(m)).ok())
}

fn merge_compat(base: Option<&ModelCompat>, over: Option<&Map<String, Value>>) -> Option<Box<ModelCompat>> {
    if over.is_none() {
        return base.cloned().map(Box::new);
    }
    map_to_compat(merge_compat_maps(compat_to_map(base), over))
}

fn merge_input_limits(
    base: Option<&ModelInputLimits>,
    over: Option<&ModelInputLimits>,
) -> Option<ModelInputLimits> {
    let Some(over) = over else { return base.cloned() };
    let base_images = base.and_then(|b| b.images.as_ref());
    let images = match &over.images {
        Some(over_images) => {
            let base_resize = base_images.and_then(|i| i.resize.as_ref());
            let resize = match &over_images.resize {
                Some(over_resize) => Some(pi_ai::providers::model_schema::ModelImageResizeOptions {
                    max_width: over_resize.max_width.or(base_resize.and_then(|r| r.max_width)),
                    max_height: over_resize.max_height.or(base_resize.and_then(|r| r.max_height)),
                    max_bytes: over_resize.max_bytes.or(base_resize.and_then(|r| r.max_bytes)),
                    jpeg_quality: over_resize
                        .jpeg_quality
                        .or(base_resize.and_then(|r| r.jpeg_quality)),
                }),
                None => base_resize.cloned(),
            };
            Some(pi_ai::providers::model_schema::ModelImageInputLimits {
                resize,
                max_per_message: over_images
                    .max_per_message
                    .or(base_images.and_then(|i| i.max_per_message)),
                max_per_request: over_images
                    .max_per_request
                    .or(base_images.and_then(|i| i.max_per_request)),
            })
        }
        None => base_images.cloned(),
    };
    Some(ModelInputLimits {
        max_request_bytes: over
            .max_request_bytes
            .or(base.and_then(|b| b.max_request_bytes)),
        images,
    })
}

fn merge_sampling_params_by_thinking_level(
    base: Option<&pi_ai::types::SamplingParamsByThinkingLevel>,
    over: Option<&pi_ai::types::SamplingParamsByThinkingLevel>,
) -> Option<pi_ai::types::SamplingParamsByThinkingLevel> {
    let Some(over) = over else { return base.cloned() };
    let mut merged = base.cloned().unwrap_or_default();
    for (level, params) in over {
        let mut entry = base.and_then(|b| b.get(level)).cloned().unwrap_or_default();
        for (k, v) in params {
            entry.insert(k.clone(), v.clone());
        }
        merged.insert(*level, entry);
    }
    Some(merged)
}

fn apply_model_override(model: &Model, over: &ModelsJsonModelOverride) -> Model {
    let mut out = model.clone();
    if let Some(name) = &over.name {
        out.name = name.clone();
    }
    if let Some(reasoning) = over.reasoning {
        out.reasoning = reasoning;
    }
    if let Some(map) = &over.thinking_level_map {
        let mut merged = model.thinking_level_map.clone().unwrap_or_default();
        merged.extend(map.iter().map(|(k, v)| (*k, v.clone())));
        out.thinking_level_map = Some(merged);
    }
    if let Some(input) = &over.input {
        out.input = input.clone();
    }
    out.input_limits = merge_input_limits(model.input_limits.as_ref(), over.input_limits.as_ref());
    if let Some(cost) = &over.cost {
        out.cost = ModelCost {
            input: cost.input.unwrap_or(model.cost.input),
            output: cost.output.unwrap_or(model.cost.output),
            cache_read: cost.cache_read.unwrap_or(model.cost.cache_read),
            cache_write: cost.cache_write.unwrap_or(model.cost.cache_write),
            tiers: cost.tiers.clone().or_else(|| model.cost.tiers.clone()),
        };
    }
    if let Some(cache) = &over.prompt_cache {
        let base = model.prompt_cache.clone().unwrap_or_default();
        out.prompt_cache = Some(pi_ai::providers::model_schema::ModelPromptCache {
            short: cache.short.or(base.short),
            long: cache.long.or(base.long),
        });
    }
    if let Some(context_window) = over.context_window {
        out.context_window = context_window as u64;
    }
    if let Some(max_tokens) = over.max_tokens {
        out.max_tokens = max_tokens as u64;
    }
    if let Some(params) = &over.sampling_params {
        let mut merged = model.sampling_params.clone().unwrap_or_default();
        for (k, v) in params {
            merged.insert(k.clone(), v.clone());
        }
        out.sampling_params = Some(merged);
    }
    out.sampling_params_by_thinking_level = merge_sampling_params_by_thinking_level(
        model.sampling_params_by_thinking_level.as_ref(),
        over.sampling_params_by_thinking_level.as_ref(),
    );
    out.compat = merge_compat(model.compat.as_deref(), over.compat.as_ref());
    out
}

fn model_from_json(
    provider_id: &str,
    definition: &ModelsJsonModel,
    provider_config: &ModelsJsonProvider,
    defaults: Option<&Model>,
) -> anyhow::Result<Model> {
    let api = definition
        .api
        .clone()
        .or_else(|| provider_config.api.clone())
        .or_else(|| defaults.map(|d| d.api.clone()))
        .filter(|api| !api.is_empty());
    let Some(api) = api else {
        anyhow::bail!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
            definition.id
        );
    };
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| provider_config.base_url.clone())
        .or_else(|| defaults.map(|d| d.base_url.clone()))
        .filter(|url| !url.is_empty());
    let Some(base_url) = base_url else {
        anyhow::bail!("Provider {provider_id}: \"baseUrl\" is required when defining custom models.");
    };
    if definition.context_window.is_some_and(|v| v <= 0.0) {
        anyhow::bail!("Provider {provider_id}, model {}: invalid contextWindow", definition.id);
    }
    if definition.max_tokens.is_some_and(|v| v <= 0.0) {
        anyhow::bail!("Provider {provider_id}, model {}: invalid maxTokens", definition.id);
    }
    Ok(Model {
        id: definition.id.clone(),
        name: definition.name.clone().unwrap_or_else(|| definition.id.clone()),
        api,
        provider: provider_id.to_string(),
        base_url,
        reasoning: definition.reasoning.unwrap_or(false),
        thinking_level_map: definition.thinking_level_map.clone(),
        input: definition
            .input
            .clone()
            .unwrap_or_else(|| vec![ModelInputModality::Text]),
        input_limits: definition.input_limits.clone(),
        cost: definition.cost.clone().unwrap_or_default(),
        prompt_cache: definition.prompt_cache.clone(),
        context_window: definition.context_window.map(|v| v as u64).unwrap_or(128000),
        max_tokens: definition.max_tokens.map(|v| v as u64).unwrap_or(16384),
        sampling_params: definition.sampling_params.clone(),
        sampling_params_by_thinking_level: definition.sampling_params_by_thinking_level.clone(),
        headers: None,
        compat: map_to_compat(merge_compat_maps(
            provider_config.compat.clone(),
            definition.compat.as_ref(),
        )),
        model_type: None,
        extra: Default::default(),
    })
}

fn find_model_defaults(models: &[AnyModel], model_id: &str, api: Option<&str>) -> Option<Model> {
    let chat: Vec<&Model> = models.iter().filter_map(AnyModel::as_chat).collect();
    chat.iter()
        .find(|m| m.id == model_id)
        .or_else(|| api.and_then(|api| chat.iter().find(|m| m.api == api)))
        .or_else(|| chat.iter().find(|m| m.api == "openai-completions"))
        .or_else(|| chat.first())
        .map(|m| (*m).clone())
}

fn definition_type(definition: &ProviderModelConfig) -> ModelType {
    match definition {
        ProviderModelConfig::Chat(_) => ModelType::Chat,
        ProviderModelConfig::Image(_) => ModelType::Image,
        ProviderModelConfig::Classifier(_) => ModelType::Classifier,
    }
}

fn definition_id(definition: &ProviderModelConfig) -> &str {
    match definition {
        ProviderModelConfig::Chat(d) => &d.id,
        ProviderModelConfig::Image(d) => &d.id,
        ProviderModelConfig::Classifier(d) => &d.id,
    }
}

fn definition_api(definition: &ProviderModelConfig) -> Option<&str> {
    match definition {
        ProviderModelConfig::Chat(d) => d.api.as_deref(),
        ProviderModelConfig::Image(d) => d.api.as_deref(),
        ProviderModelConfig::Classifier(d) => d.api.as_deref(),
    }
    .filter(|api| !api.is_empty())
}

fn definition_base_url(definition: &ProviderModelConfig) -> Option<&str> {
    match definition {
        ProviderModelConfig::Chat(d) => d.base_url.as_deref(),
        ProviderModelConfig::Image(d) => d.base_url.as_deref(),
        ProviderModelConfig::Classifier(d) => d.base_url.as_deref(),
    }
    .filter(|url| !url.is_empty())
}

fn definition_headers(definition: &ProviderModelConfig) -> Option<&Headers> {
    match definition {
        ProviderModelConfig::Chat(d) => d.headers.as_ref(),
        ProviderModelConfig::Image(d) => d.headers.as_ref(),
        ProviderModelConfig::Classifier(d) => d.headers.as_ref(),
    }
}

fn find_extension_model_defaults<'a>(
    models: &'a [AnyModel],
    definition: &ProviderModelConfig,
) -> Option<&'a AnyModel> {
    let kind = definition_type(definition);
    let candidates: Vec<&AnyModel> = models.iter().filter(|m| m.model_type() == kind).collect();
    candidates
        .iter()
        .find(|m| m.id() == definition_id(definition))
        .or_else(|| {
            definition_api(definition).and_then(|api| candidates.iter().find(|m| m.api() == api))
        })
        .or_else(|| {
            (kind == ModelType::Chat)
                .then(|| candidates.iter().find(|m| m.api() == "openai-completions"))
                .flatten()
        })
        .or_else(|| candidates.first())
        .copied()
}

fn extension_model_from_definition(
    provider_id: &str,
    models: &[AnyModel],
    config: &ProviderConfigInput,
    definition: &ProviderModelConfig,
) -> anyhow::Result<AnyModel> {
    let kind = definition_type(definition);
    let defaults = find_extension_model_defaults(models, definition);
    let api = definition_api(definition)
        .map(str::to_string)
        .or_else(|| {
            (kind == ModelType::Chat)
                .then(|| config.api.clone())
                .flatten()
                .filter(|api| !api.is_empty())
        })
        .or_else(|| defaults.map(|d| d.api().to_string()));
    let Some(api) = api else {
        anyhow::bail!(
            "Provider {provider_id}, model {}: no \"api\" specified. Set it at model level{}.",
            definition_id(definition),
            if kind == ModelType::Chat { " or provider level" } else { "" }
        );
    };
    let base_url = definition_base_url(definition)
        .map(str::to_string)
        .or_else(|| config.base_url.clone().filter(|url| !url.is_empty()))
        .or_else(|| defaults.map(|d| d.base_url().to_string()));
    let Some(base_url) = base_url else {
        anyhow::bail!("Provider {provider_id}: \"baseUrl\" is required when defining custom models.");
    };
    let mut value = serde_json::to_value(definition)?;
    if let Value::Object(map) = &mut value {
        map.insert("api".to_string(), Value::String(api));
        map.insert("provider".to_string(), Value::String(provider_id.to_string()));
        map.insert("baseUrl".to_string(), Value::String(base_url));
        map.remove("headers");
    }
    AnyModel::from_value(value)
        .map_err(|error| anyhow::anyhow!("Provider {provider_id}, model {}: {error}", definition_id(definition)))
}

fn apply_models_json(
    provider_id: &str,
    base_models: Vec<AnyModel>,
    config: Option<&ModelsJsonProvider>,
) -> anyhow::Result<Vec<AnyModel>> {
    let Some(config) = config else { return Ok(base_models) };
    if config.oauth.is_some() && config.base_url.is_none() {
        anyhow::bail!("Provider {provider_id}: \"baseUrl\" is required when \"oauth\" is set.");
    }
    let has_overrides = config.model_overrides.as_ref().is_some_and(|o| !o.is_empty());
    if config.models.as_ref().is_none_or(|m| m.is_empty())
        && config.base_url.is_none()
        && config.headers.is_none()
        && config.compat.is_none()
        && !has_overrides
        && config.api_key.is_none()
        && config.oauth.is_none()
        && config.auth_header.is_none()
    {
        anyhow::bail!(
            "Provider {provider_id}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        );
    }

    let radius = config.oauth.as_deref() == Some("radius");
    let mut models: Vec<AnyModel> = base_models
        .into_iter()
        .map(|mut model| {
            if !radius && let Some(base_url) = &config.base_url {
                model.set_base_url(base_url.clone());
            }
            match &mut model {
                AnyModel::Chat(chat) => {
                    chat.compat = merge_compat(chat.compat.as_deref(), config.compat.as_ref());
                }
                // Classifier APIs read their own compat fields, such as how a System One service
                // takes images. A shallow spread, unlike chat compat.
                AnyModel::Classifier(classifier) => {
                    if let Some(over) = config.compat.as_ref() {
                        let mut merged = compat_to_map(classifier.compat.as_deref()).unwrap_or_default();
                        for (key, value) in over {
                            merged.insert(key.clone(), value.clone());
                        }
                        classifier.compat = map_to_compat(Some(merged));
                    }
                }
                AnyModel::Image(_) => {}
            }
            model
        })
        .collect();
    for definition in config.models.iter().flatten() {
        let existing = models
            .iter()
            .position(|m| m.model_type() == ModelType::Chat && m.id() == definition.id);
        let api = definition.api.as_deref().or(config.api.as_deref());
        let defaults = find_model_defaults(&models, &definition.id, api);
        let model = model_from_json(provider_id, definition, config, defaults.as_ref())?;
        match existing {
            Some(index) => models[index] = AnyModel::Chat(model),
            None => models.push(AnyModel::Chat(model)),
        }
    }
    Ok(models)
}

fn apply_extension(
    provider_id: &str,
    models: Vec<AnyModel>,
    config: Option<&ProviderConfigInput>,
    models_override: Option<&[ProviderModelConfig]>,
) -> anyhow::Result<Vec<AnyModel>> {
    let Some(config) = config else { return Ok(models) };
    let definitions = models_override.or(config.models.as_deref());
    let Some(definitions) = definitions else {
        return Ok(match &config.base_url {
            Some(base_url) => models
                .into_iter()
                .map(|mut m| {
                    m.set_base_url(base_url.clone());
                    m
                })
                .collect(),
            None => models,
        });
    };
    definitions
        .iter()
        .map(|definition| extension_model_from_definition(provider_id, &models, config, definition))
        .collect()
}

// ---------------------------------------------------------------------------
// Auth composition
// ---------------------------------------------------------------------------

/// Bridges the legacy extension OAuth callbacks onto a pi-ai auth interaction.
struct LegacyCallbacks {
    interaction: ProviderAuthInteraction,
}

#[async_trait]
impl OAuthLoginCallbacks for LegacyCallbacks {
    fn on_auth(&self, info: OAuthAuthInfo) {
        self.interaction.interaction.notify(AuthEvent::AuthUrl {
            url: info.url,
            instructions: info.instructions,
        });
    }
    fn on_device_code(&self, info: OAuthDeviceCodeInfo) {
        self.interaction.interaction.notify(AuthEvent::DeviceCode {
            user_code: info.user_code,
            verification_uri: info.verification_uri,
            interval_seconds: info.interval_seconds,
            expires_in_seconds: info.expires_in_seconds,
        });
    }
    async fn on_prompt(&self, prompt: OAuthPrompt) -> anyhow::Result<String> {
        self.interaction
            .interaction
            .prompt(AuthPrompt {
                kind: AuthPromptKind::Text,
                message: prompt.message,
                placeholder: prompt.placeholder,
                options: Vec::new(),
                signal: None,
            })
            .await
    }
    fn on_progress(&self, message: &str) {
        self.interaction.interaction.notify(AuthEvent::Progress {
            message: message.to_string(),
        });
    }
    async fn on_manual_code_input(&self) -> Option<anyhow::Result<String>> {
        Some(
            self.interaction
                .interaction
                .prompt(AuthPrompt {
                    kind: AuthPromptKind::ManualCode,
                    message: "Paste the authorization code".to_string(),
                    placeholder: None,
                    options: Vec::new(),
                    signal: None,
                })
                .await,
        )
    }
    async fn on_select(&self, prompt: OAuthSelectPrompt) -> anyhow::Result<Option<String>> {
        let selected = self
            .interaction
            .interaction
            .prompt(AuthPrompt {
                kind: AuthPromptKind::Select,
                message: prompt.message,
                placeholder: None,
                options: prompt
                    .options
                    .into_iter()
                    .map(|o| AuthSelectOption { id: o.id, label: o.label, description: None })
                    .collect(),
                signal: None,
            })
            .await?;
        Ok(Some(selected))
    }
    fn signal(&self) -> Option<CancellationToken> {
        Some(self.interaction.signal.clone())
    }
}

/// `adaptOAuth`: a legacy extension OAuth config as canonical `OAuthAuth`.
struct AdaptedOAuth {
    config: ExtensionOAuthConfig,
}

#[async_trait]
impl OAuthAuth for AdaptedOAuth {
    fn name(&self) -> &str {
        &self.config.name
    }
    fn is_subscription(&self) -> Option<bool> {
        self.config.is_subscription
    }
    async fn login(
        &self,
        interaction: ProviderAuthInteraction,
        _options: Option<LoginOptions>,
    ) -> anyhow::Result<OAuthCredentials> {
        (self.config.login)(Arc::new(LegacyCallbacks { interaction })).await
    }
    async fn refresh(&self, credential: OAuthCredentials, signal: CancellationToken) -> anyhow::Result<OAuthCredentials> {
        (self.config.refresh_token)(credential, signal).await
    }
    async fn to_auth(&self, credential: &OAuthCredentials) -> anyhow::Result<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some((self.config.get_api_key)(credential)),
            headers: None,
            base_url: None,
        })
    }
}

fn with_configured_auth(auth: ModelAuth, headers: Option<Headers>, auth_header: bool) -> anyhow::Result<ModelAuth> {
    let mut merged: Option<ProviderHeaders> = if auth.headers.is_some() || headers.is_some() {
        let mut merged = auth.headers.clone().unwrap_or_default();
        for (k, v) in headers.iter().flatten() {
            merged.insert(k.clone(), Some(v.clone()));
        }
        Some(merged)
    } else {
        None
    };
    if auth_header {
        let Some(api_key) = auth.api_key.as_deref().filter(|k| !k.is_empty()) else {
            anyhow::bail!("authHeader requires a resolved API key");
        };
        merged
            .get_or_insert_with(ProviderHeaders::new)
            .insert("Authorization".to_string(), Some(format!("Bearer {api_key}")));
    }
    Ok(ModelAuth { headers: merged, ..auth })
}

fn configured_api_key(config: Option<&ModelsJsonProvider>, extension: Option<&ProviderConfigInput>) -> Option<String> {
    extension
        .and_then(|e| e.api_key.clone())
        .or_else(|| config.and_then(|c| c.api_key.clone()))
}

fn configured_headers(config: Option<&ModelsJsonProvider>, extension: Option<&ProviderConfigInput>) -> Option<Headers> {
    let config_headers = config.and_then(|c| c.headers.as_ref());
    let extension_headers = extension.and_then(|e| e.headers.as_ref());
    if config_headers.is_none() && extension_headers.is_none() {
        return None;
    }
    let mut headers = Headers::new();
    for source in [config_headers, extension_headers].into_iter().flatten() {
        for (k, v) in source {
            headers.insert(k.clone(), v.clone());
        }
    }
    Some(headers)
}

fn configured_auth_header(config: Option<&ModelsJsonProvider>, extension: Option<&ProviderConfigInput>) -> bool {
    extension
        .and_then(|e| e.auth_header)
        .or_else(|| config.and_then(|c| c.auth_header))
        .unwrap_or(false)
}

async fn config_context_env(
    values: &[String],
    ctx: &Arc<dyn AuthContext>,
    explicit: Option<IndexMap<String, String>>,
) -> Option<IndexMap<String, String>> {
    let mut env = explicit.unwrap_or_default();
    let names: IndexSet<String> = values
        .iter()
        .flat_map(|value| get_config_value_env_var_names(value))
        .collect();
    for name in names {
        if env.contains_key(&name) {
            continue;
        }
        if let Some(value) = ctx.env(&name).await {
            env.insert(name, value);
        }
    }
    (!env.is_empty()).then_some(env)
}

/// Api-key auth composed from the inherited auth, a configured key and configured headers.
struct ComposedApiKeyAuth {
    provider_id: String,
    name: String,
    inherited: Option<Arc<dyn ApiKeyAuth>>,
    raw_key: Option<String>,
    raw_headers: Option<Headers>,
    auth_header: bool,
}

impl ComposedApiKeyAuth {
    async fn inherited_check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        let Some(inherited) = &self.inherited else { return Ok(None) };
        if inherited.supports_check() {
            return inherited.check(input).await;
        }
        let resolved = inherited.resolve(input).await?;
        Ok(resolved.map(|r| AuthCheck { source: r.source, auth_type: AuthType::ApiKey }))
    }
}

#[async_trait]
impl ApiKeyAuth for ComposedApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }
    fn supports_login(&self) -> bool {
        true
    }
    async fn login(&self, interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        if let Some(inherited) = self.inherited.as_ref().filter(|a| a.supports_login()) {
            return inherited.login(interaction).await;
        }
        let key = interaction.interaction.prompt(AuthPrompt::secret("Enter API key")).await?;
        Ok(ApiKeyCredential { key: Some(key), env: None })
    }
    fn supports_check(&self) -> bool {
        true
    }
    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        if let Some(credential) = &input.credential {
            if self.inherited.as_ref().is_some_and(|a| a.supports_check()) {
                return self.inherited_check(input).await;
            }
            if credential.key.as_deref().is_some_and(|k| !k.is_empty()) {
                return Ok(Some(AuthCheck {
                    source: Some("stored credential".to_string()),
                    auth_type: AuthType::ApiKey,
                }));
            }
            return self.inherited_check(input).await;
        }
        if let Some(raw_key) = &self.raw_key {
            if is_command_config_value(raw_key) {
                return Ok(Some(AuthCheck {
                    source: Some("configured API key".to_string()),
                    auth_type: AuthType::ApiKey,
                }));
            }
            for name in get_config_value_env_var_names(raw_key) {
                if input.ctx.env(&name).await.is_none() {
                    return Ok(None);
                }
            }
            return Ok(Some(AuthCheck {
                source: Some("configured API key".to_string()),
                auth_type: AuthType::ApiKey,
            }));
        }
        self.inherited_check(input).await
    }
    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        let result = if let Some(credential) = &input.credential {
            match &self.inherited {
                Some(inherited) => inherited.resolve(input.clone()).await?,
                None => credential.key.clone().filter(|k| !k.is_empty()).map(|key| AuthResult {
                    auth: ModelAuth { api_key: Some(key), headers: None, base_url: None },
                    env: credential.env.clone(),
                    source: Some("stored credential".to_string()),
                }),
            }
        } else if let Some(raw_key) = &self.raw_key {
            let env = config_context_env(std::slice::from_ref(raw_key), &input.ctx, None).await;
            let key = resolve_config_value_or_throw(
                raw_key,
                &format!("API key for provider \"{}\"", self.provider_id),
                env,
            )?;
            match &self.inherited {
                Some(inherited) => {
                    inherited
                        .resolve(ApiKeyResolveInput {
                            credential: Some(ApiKeyCredential { key: Some(key), env: None }),
                            ..input.clone()
                        })
                        .await?
                }
                None => Some(AuthResult {
                    auth: ModelAuth { api_key: Some(key), headers: None, base_url: None },
                    env: None,
                    source: Some("configured API key".to_string()),
                }),
            }
        } else {
            match &self.inherited {
                Some(inherited) => inherited.resolve(input.clone()).await?,
                None => None,
            }
        };
        let Some(result) = result else { return Ok(None) };
        let mut explicit_env = IndexMap::new();
        if let Some(env) = input.credential.as_ref().and_then(|c| c.env.as_ref()) {
            explicit_env.extend(env_to_index_map(env));
        }
        if let Some(env) = &result.env {
            explicit_env.extend(env_to_index_map(env));
        }
        let header_values: Vec<String> = self.raw_headers.iter().flat_map(|h| h.values().cloned()).collect();
        let header_env = config_context_env(&header_values, &input.ctx, Some(explicit_env)).await;
        let headers = resolve_headers_or_throw(
            self.raw_headers.as_ref(),
            &format!("provider \"{}\"", self.provider_id),
            header_env.as_ref(),
        )?;
        let auth = with_configured_auth(result.auth.clone(), headers, self.auth_header)?;
        Ok(Some(AuthResult { auth, ..result }))
    }
}

fn compose_api_key_auth(
    provider_id: &str,
    base: Option<&Arc<dyn Provider>>,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<Arc<dyn ApiKeyAuth>> {
    let inherited = base.and_then(|b| b.auth().api_key.clone());
    let raw_key = configured_api_key(config, extension);
    let has_oauth = extension.is_some_and(|e| e.oauth.is_some()) || base.is_some_and(|b| b.auth().oauth.is_some());
    // OAuth-only providers get no fabricated API-key login method.
    if inherited.is_none() && raw_key.is_none() && has_oauth {
        return None;
    }
    Some(Arc::new(ComposedApiKeyAuth {
        provider_id: provider_id.to_string(),
        name: inherited.as_ref().map(|a| a.name().to_string()).unwrap_or_else(|| "API key".to_string()),
        inherited,
        raw_key,
        raw_headers: configured_headers(config, extension),
        auth_header: configured_auth_header(config, extension),
    }))
}

/// OAuth auth with configured headers applied to the derived request auth.
struct ComposedOAuth {
    provider_id: String,
    inner: Arc<dyn OAuthAuth>,
    raw_headers: Option<Headers>,
    auth_header: bool,
}

#[async_trait]
impl OAuthAuth for ComposedOAuth {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn is_subscription(&self) -> Option<bool> {
        self.inner.is_subscription()
    }
    fn login_label(&self) -> Option<&str> {
        self.inner.login_label()
    }
    async fn login(
        &self,
        interaction: ProviderAuthInteraction,
        options: Option<LoginOptions>,
    ) -> anyhow::Result<OAuthCredentials> {
        self.inner.login(interaction, options).await
    }
    async fn refresh(&self, credential: OAuthCredentials, signal: CancellationToken) -> anyhow::Result<OAuthCredentials> {
        self.inner.refresh(credential, signal).await
    }
    async fn to_auth(&self, credential: &OAuthCredentials) -> anyhow::Result<ModelAuth> {
        let auth = self.inner.to_auth(credential).await?;
        let env: Option<IndexMap<String, String>> = credential.extra.get("env").and_then(|env| {
            env.as_object().map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
        });
        let headers = resolve_headers_or_throw(
            self.raw_headers.as_ref(),
            &format!("provider \"{}\"", self.provider_id),
            env.as_ref(),
        )?;
        with_configured_auth(auth, headers, self.auth_header)
    }
}

fn compose_oauth_auth(
    provider_id: &str,
    base: Option<&Arc<dyn Provider>>,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<Arc<dyn OAuthAuth>> {
    let oauth: Arc<dyn OAuthAuth> = match extension.and_then(|e| e.oauth.clone()) {
        Some(config) => Arc::new(AdaptedOAuth { config }),
        None => base.and_then(|b| b.auth().oauth.clone())?,
    };
    Some(Arc::new(ComposedOAuth {
        provider_id: provider_id.to_string(),
        inner: oauth,
        raw_headers: configured_headers(config, extension),
        auth_header: configured_auth_header(config, extension),
    }))
}

fn raw_model_headers(
    model: &AnyModel,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<Headers> {
    // models.json definitions and overrides are chat-only. Extension definitions
    // are matched by operation and id so colliding models cannot share headers.
    let is_chat = model.model_type() == ModelType::Chat;
    let chat_definition = if is_chat {
        config.and_then(|c| c.models.as_ref()).and_then(|models| models.iter().find(|m| m.id == model.id()))
    } else {
        None
    };
    let extension_model = extension.and_then(|e| e.models.as_ref()).and_then(|models| {
        models
            .iter()
            .find(|m| definition_type(m) == model.model_type() && definition_id(m) == model.id())
    });
    let override_headers = if is_chat {
        config
            .and_then(|c| c.model_overrides.as_ref())
            .and_then(|o| o.get(model.id()))
            .and_then(|o| o.headers.as_ref())
    } else {
        None
    };
    let mut headers = Headers::new();
    for source in [
        override_headers,
        chat_definition.and_then(|d| d.headers.as_ref()),
        extension_model.and_then(definition_headers),
    ]
    .into_iter()
    .flatten()
    {
        for (k, v) in source {
            headers.insert(k.clone(), v.clone());
        }
    }
    (!headers.is_empty()).then_some(headers)
}

pub fn validate_extension_provider(
    provider_id: &str,
    base: Option<Arc<dyn Provider>>,
    models_config: Option<&ModelsJsonProvider>,
    extension: &ProviderConfigInput,
) -> anyhow::Result<()> {
    if extension.stream_simple.is_some() && extension.api.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("Provider {provider_id}: \"api\" is required when registering streamSimple.");
    }
    let models = apply_models_json(provider_id, get_all_provider_models(base.as_ref()), models_config)?;
    apply_extension(provider_id, models, Some(extension), None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Composed provider
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ComposedState {
    extension_oauth_credential: Option<OAuthCredentials>,
    refreshed_extension_models: Option<Vec<ProviderModelConfig>>,
}

struct ComposedProvider {
    id: String,
    name: String,
    base_url: Option<String>,
    auth: ProviderAuth,
    base: Option<Arc<dyn Provider>>,
    config: Option<ModelsJsonProvider>,
    extension: Option<ProviderConfigInput>,
    state: Arc<Mutex<ComposedState>>,
    supports_refresh: bool,
}

impl ComposedProvider {
    // models.json modelOverrides are the topmost user-config layer: they apply once,
    // after custom-model upserts, extension model replacement, and legacy OAuth projection.
    fn compute_all_models(&self) -> anyhow::Result<Vec<AnyModel>> {
        let (refreshed, credential) = {
            let state = self.state.lock();
            (state.refreshed_extension_models.clone(), state.extension_oauth_credential.clone())
        };
        let base_models = apply_models_json(&self.id, get_all_provider_models(self.base.as_ref()), self.config.as_ref())?;
        let mut models = apply_extension(
            &self.id,
            base_models,
            self.extension.as_ref(),
            if self.extension.is_some() { refreshed.as_deref() } else { None },
        )?;
        if let Some(credential) = &credential
            && let Some(modify) = self.extension.as_ref().and_then(|e| e.oauth.as_ref()).and_then(|o| o.modify_models.clone())
        {
            // The extension hook is chat-only; other model types pass through untouched.
            let chat: Vec<Model> = models.iter().filter_map(|m| m.as_chat().cloned()).collect();
            let others: Vec<AnyModel> = models.iter().filter(|m| m.model_type() != ModelType::Chat).cloned().collect();
            models = modify(chat, credential).into_iter().map(AnyModel::Chat).chain(others).collect();
        }
        let overrides = self.config.as_ref().and_then(|c| c.model_overrides.as_ref());
        Ok(models
            .into_iter()
            .map(|model| match (overrides.and_then(|o| o.get(model.id())), model) {
                (Some(over), AnyModel::Chat(chat)) => AnyModel::Chat(apply_model_override(&chat, over)),
                (_, model) => model,
            })
            .collect())
    }

    fn supports_base_api(&self, model: &Model) -> bool {
        self.base
            .as_ref()
            .is_some_and(|base| base.get_models().iter().any(|entry| entry.api == model.api))
    }

    fn stream_with(&self, model: &Model, context: &TranscriptContext, options: StreamOptions, simple: bool) -> AssistantMessageEventStream {
        if let Some(extension) = &self.extension
            && let Some(stream_simple) = &extension.stream_simple
            && extension.api.as_deref() == Some(model.api.as_str())
        {
            return stream_simple(model.clone(), context.clone(), Some(options));
        }
        if let Some(base) = &self.base
            && self.supports_base_api(model)
        {
            return if simple {
                base.stream_simple(model, context, options)
            } else {
                base.stream(model, context, options)
            };
        }
        let Some(api) = get_api_provider(&model.api) else {
            return error_stream(model, anyhow::anyhow!("No API provider registered for api: {}", model.api));
        };
        if simple {
            api.streams.stream_simple(model, context, options)
        } else {
            api.streams.stream(model, context, options)
        }
    }
}

#[async_trait]
impl Provider for ComposedProvider {
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
        self.base.as_ref().and_then(|b| b.headers())
    }
    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }
    fn get_models(&self) -> Vec<Model> {
        self.get_all_models().into_iter().filter_map(AnyModel::into_chat).collect()
    }
    fn get_all_models(&self) -> Vec<AnyModel> {
        self.compute_all_models().unwrap_or_default()
    }
    fn supports_refresh(&self) -> bool {
        self.supports_refresh
    }
    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        if !self.supports_refresh {
            return Ok(());
        }
        if let Some(base) = &self.base {
            base.refresh_models(context.clone()).await?;
        }
        let mut refreshed: Option<Vec<ProviderModelConfig>> = None;
        if let Some(refresh) = self.extension.as_ref().and_then(|e| e.refresh_models.clone()) {
            refreshed = Some(refresh(context.clone()).await?);
        }
        if context.signal.is_cancelled() {
            return Ok(());
        }
        let oauth_credential = match &context.credential {
            Some(Credential::OAuth(credential)) => Some(credential.clone()),
            _ => None,
        };
        if let Some(refreshed) = &refreshed {
            // Validate before publishing the new synchronous list.
            let models = apply_models_json(&self.id, get_all_provider_models(self.base.as_ref()), self.config.as_ref())?;
            apply_extension(&self.id, models, Some(self.extension.as_ref().expect("refresh requires extension")), Some(refreshed))?;
        }
        let state = self.state.clone();
        (context.publish)(ModelsPublication {
            persist: None,
            update: Some(Box::new(move || {
                let mut state = state.lock();
                if let Some(refreshed) = refreshed {
                    state.refreshed_extension_models = Some(refreshed);
                }
                state.extension_oauth_credential = oauth_credential;
            })),
        })
        .await?;
        Ok(())
    }
    fn filter_models(&self, models: &[Model], credential: Option<&Credential>) -> Option<Vec<Model>> {
        self.base.as_ref().and_then(|b| b.filter_models(models, credential))
    }
    fn filter_all_models(&self, models: &[AnyModel], credential: Option<&Credential>) -> Option<Vec<AnyModel>> {
        self.base.as_ref().and_then(|b| b.filter_all_models(models, credential))
    }
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.stream_with(model, context, options, false)
    }
    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.stream_with(model, context, options, true)
    }
    fn supports_fetch_deferred(&self) -> bool {
        self.base.as_ref().is_some_and(|b| b.supports_fetch_deferred())
    }
    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        match self.base.as_ref().filter(|b| b.supports_fetch_deferred()) {
            Some(base) => base.fetch_deferred(model, handle, options),
            None => error_stream(
                model,
                anyhow::anyhow!("Provider {} does not support deferred responses", model.provider),
            ),
        }
    }
    fn supports_cancel_deferred(&self) -> bool {
        self.base.as_ref().is_some_and(|b| b.supports_cancel_deferred())
    }
    async fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> anyhow::Result<()> {
        match self.base.as_ref().filter(|b| b.supports_cancel_deferred()) {
            Some(base) => base.cancel_deferred(model, handle, options).await,
            None => anyhow::bail!("Provider {} does not support deferred responses", model.provider),
        }
    }
    fn supports_images(&self) -> bool {
        self.base.as_ref().is_some_and(|b| b.supports_images())
            || self.extension.as_ref().and_then(|e| e.images.as_ref()).is_some_and(|i| !i.is_empty())
    }
    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        let implementation = self.extension.as_ref().and_then(|e| e.images.as_ref()).and_then(|i| i.get(&model.api)).cloned();
        if let Some(implementation) = implementation {
            return implementation.generate_images(model, context, options).await;
        }
        if let Some(base) = self.base.as_ref().filter(|b| b.supports_images()) {
            return base.generate_images(model, context, options).await;
        }
        image_error_result(
            model,
            &anyhow::anyhow!("Provider {} has no image implementation for \"{}\"", self.id, model.api),
            false,
        )
    }
    fn supports_classify(&self) -> bool {
        self.base.as_ref().is_some_and(|b| b.supports_classify())
            || self.extension.as_ref().and_then(|e| e.classifiers.as_ref()).is_some_and(|c| !c.is_empty())
    }
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        let implementation = self.extension.as_ref().and_then(|e| e.classifiers.as_ref()).and_then(|c| c.get(&model.api)).cloned();
        if let Some(implementation) = implementation {
            return implementation.classify(model, context, options).await;
        }
        if let Some(base) = self.base.as_ref().filter(|b| b.supports_classify()) {
            return base.classify(model, context, options).await;
        }
        classifier_error_result(
            model,
            &anyhow::anyhow!("Provider {} has no classifier implementation for \"{}\"", self.id, model.api),
            false,
        )
    }
}

/// Compose built-in, models.json, and extension layers without reading credentials.
pub fn compose_model_provider(
    provider_id: &str,
    base: Option<Arc<dyn Provider>>,
    model_config: Arc<ModelConfig>,
    extension: Option<&ProviderConfigInput>,
) -> anyhow::Result<Arc<dyn Provider>> {
    let config = model_config.get_provider(provider_id).cloned();
    let api_key = compose_api_key_auth(provider_id, base.as_ref(), config.as_ref(), extension);
    let oauth = compose_oauth_auth(provider_id, base.as_ref(), config.as_ref(), extension);
    let supports_refresh = base.as_ref().is_some_and(|b| b.supports_refresh())
        || extension.is_some_and(|e| e.refresh_models.is_some())
        || extension.and_then(|e| e.oauth.as_ref()).is_some_and(|o| o.modify_models.is_some());
    let name = extension
        .and_then(|e| e.name.clone())
        .or_else(|| config.as_ref().and_then(|c| c.name.clone()))
        .or_else(|| base.as_ref().map(|b| b.name().to_string()))
        .or_else(|| extension.and_then(|e| e.oauth.as_ref()).map(|o| o.name.clone()))
        .unwrap_or_else(|| provider_id.to_string());
    let base_url = extension
        .and_then(|e| e.base_url.clone())
        .or_else(|| config.as_ref().and_then(|c| c.base_url.clone()))
        .or_else(|| base.as_ref().and_then(|b| b.base_url().map(str::to_string)));
    let provider = ComposedProvider {
        id: provider_id.to_string(),
        name,
        base_url,
        auth: ProviderAuth { api_key: api_key.clone(), oauth: oauth.clone() },
        base,
        config,
        extension: extension.cloned(),
        state: Arc::default(),
        supports_refresh,
    };
    // Validate eagerly so registration/reload reports structural errors immediately.
    provider.compute_all_models()?;
    if api_key.is_none() && oauth.is_none() {
        anyhow::bail!("Provider {provider_id}: no authentication method configured.");
    }
    Ok(Arc::new(provider))
}

pub fn resolve_configured_model_headers(
    model: &AnyModel,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
    env: Option<&IndexMap<String, String>>,
) -> anyhow::Result<Option<IndexMap<String, String>>> {
    resolve_headers_or_throw(
        raw_model_headers(model, config, extension).as_ref(),
        &format!("model \"{}/{}\"", model.provider(), model.id()),
        env,
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityRequestConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    pub auth_header: bool,
}

pub fn resolve_compatibility_request_config(
    model: &AnyModel,
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> anyhow::Result<CompatibilityRequestConfig> {
    let mut raw = configured_headers(config, extension).unwrap_or_default();
    for (k, v) in raw_model_headers(model, config, extension).unwrap_or_default() {
        raw.insert(k, v);
    }
    let configured = resolve_headers_or_throw(
        Some(&raw),
        &format!("model \"{}/{}\"", model.provider(), model.id()),
        None,
    )?;
    let headers = if model.headers().is_some() || configured.is_some() {
        let mut headers = model.headers().map(to_provider_headers).unwrap_or_default();
        for (k, v) in configured.iter().flatten() {
            headers.insert(k.clone(), Some(v.clone()));
        }
        Some(headers)
    } else {
        None
    };
    Ok(CompatibilityRequestConfig {
        headers,
        auth_header: configured_auth_header(config, extension),
    })
}

pub fn configured_request_auth_status(
    config: Option<&ModelsJsonProvider>,
    extension: Option<&ProviderConfigInput>,
) -> Option<AuthStatus> {
    let value = configured_api_key(config, extension)?;
    if is_command_config_value(&value) {
        return Some(AuthStatus::configured(AuthStatusSource::ModelsJsonCommand, None));
    }
    let names = get_config_value_env_var_names(&value);
    if !names.is_empty() {
        return Some(if is_config_value_configured(&value, None) {
            AuthStatus::configured(AuthStatusSource::Environment, Some(names.join(", ")))
        } else {
            AuthStatus::unconfigured()
        });
    }
    Some(AuthStatus::configured(
        if extension.is_some_and(|e| e.api_key.is_some()) {
            AuthStatusSource::Fallback
        } else {
            AuthStatusSource::ModelsJsonKey
        },
        None,
    ))
}
