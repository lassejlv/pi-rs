//! Port of `packages/coding-agent/src/core/model-registry.ts`.

use crate::compat::AuthOperationOptions;
use crate::core::model_runtime::{ModelRuntime, ModelRuntimeGetAuthProviderOrModel};
use crate::core::provider_composer::AuthStatus;
use crate::core::virtual_models::VirtualModelDefinition;
use indexmap::IndexMap;
use pi_ai::auth::types::AuthResult;
use pi_ai::models::{ModelsError, ModelsRefreshOptions, ModelsRefreshResult, ModelsRequestOptions, Provider};
use pi_ai::types::{
    AnyModel, AssistantImages, AssistantMessage, ClassifierContext, ClassifierModel, ClassifierResult,
    Context, ImageModel, ImagesContext, Model, ModelType, ProviderHeaders,
};
use pi_ai::utils::event_stream::AssistantMessageEventStream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use crate::core::provider_composer::ProviderConfigInput;
pub use crate::core::provider_composer::clear_api_key_cache;

/// Successful request auth resolution (`{ ok: true, ... }`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRequestAuthOk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
}

/// `ResolvedRequestAuth`: `{ ok: true, apiKey?, headers?, baseUrl?, env? } | { ok: false, error }`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedRequestAuth {
    Ok(ResolvedRequestAuthOk),
    Err { error: String },
}

impl ResolvedRequestAuth {
    pub fn ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

impl Serialize for ResolvedRequestAuth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let mut value = match self {
            Self::Ok(auth) => serde_json::to_value(auth).map_err(S::Error::custom)?,
            Self::Err { error } => serde_json::json!({ "error": error }),
        };
        if let serde_json::Value::Object(map) = &mut value {
            map.shift_insert(0, "ok".to_string(), serde_json::Value::Bool(self.ok()));
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResolvedRequestAuth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            serde_json::from_value(value).map(Self::Ok).map_err(D::Error::custom)
        } else {
            let error = value.get("error").and_then(serde_json::Value::as_str).unwrap_or_default();
            Ok(Self::Err { error: error.to_string() })
        }
    }
}

#[derive(Clone)]
pub enum ModelRegistryRegisterProviderProviderOrName {
    Arc(Arc<dyn Provider>),
    String(String),
}

/// Synchronous compatibility facade exposed to extensions.
/// Coding-agent internals use ModelRuntime directly.
pub struct ModelRegistry {
    runtime: Arc<ModelRuntime>,
}

impl ModelRegistry {
    pub fn new(runtime: Arc<ModelRuntime>) -> Self {
        Self { runtime }
    }

    /// The wrapped runtime.
    pub fn runtime(&self) -> &Arc<ModelRuntime> {
        &self.runtime
    }

    /// Reload models.json asynchronously. Await before making synchronous registry reads.
    pub async fn refresh(&self, options: Option<ModelsRefreshOptions>) -> anyhow::Result<ModelsRefreshResult> {
        self.runtime.refresh(options).await
    }

    pub fn get_error(&self) -> Option<String> {
        self.runtime.get_error()
    }

    pub fn get_all(&self) -> Vec<Model> {
        self.runtime.get_models(None)
    }

    pub fn get_available(&self) -> Vec<Model> {
        self.runtime.get_available_snapshot()
    }

    pub fn find(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.runtime.get_model(provider, model_id)
    }

    /// Find a model of a non-chat type, e.g. `findOfType("classifier", "typesafe", "jev-latest")`.
    pub fn find_of_type(&self, r#type: ModelType, provider: &str, model_id: &str) -> Option<AnyModel> {
        self.runtime.get_model_of_type(r#type, provider, model_id)
    }

    pub fn has_configured_auth(&self, model: &Model) -> bool {
        self.runtime.has_configured_auth(&model.provider)
    }

    pub async fn get_api_key_and_headers(&self, model: &Model) -> ResolvedRequestAuth {
        let result: anyhow::Result<ResolvedRequestAuth> = async {
            let resolution = self
                .runtime
                .get_auth(&ModelRuntimeGetAuthProviderOrModel::AnyModel(AnyModel::Chat(model.clone())), None)
                .await?;
            let Some(resolution) = resolution else {
                let compatibility = self.runtime.get_compatibility_request_config(model)?;
                if compatibility.auth_header {
                    return Ok(ResolvedRequestAuth::Err {
                        error: format!("No API key found for \"{}\"", model.provider),
                    });
                }
                return Ok(ResolvedRequestAuth::Ok(ResolvedRequestAuthOk {
                    headers: compatibility.headers,
                    ..Default::default()
                }));
            };
            Ok(ResolvedRequestAuth::Ok(ResolvedRequestAuthOk {
                api_key: resolution.auth.api_key,
                headers: resolution.auth.headers,
                base_url: resolution.auth.base_url.filter(|url| !url.is_empty()),
                env: resolution.env.map(|env| env.into_iter().collect()),
            }))
        }
        .await;
        match result {
            Ok(auth) => auth,
            Err(error) => {
                let message = error
                    .downcast_ref::<ModelsError>()
                    .and_then(|e| e.cause.clone())
                    .unwrap_or_else(|| error.to_string());
                ResolvedRequestAuth::Err {
                    error: if message == "authHeader requires a resolved API key" {
                        format!("No API key found for \"{}\"", model.provider)
                    } else {
                        message
                    },
                }
            }
        }
    }

    pub fn get_provider_auth_status(&self, provider: &str) -> AuthStatus {
        self.runtime.get_provider_auth_status(provider)
    }

    pub fn get_provider(&self, provider: &str) -> Option<Arc<dyn Provider>> {
        self.runtime.get_provider(provider)
    }

    /// Stream through the configured provider with request-time authentication.
    pub fn stream(&self, model: Model, context: Context, options: Option<ModelsRequestOptions>) -> AssistantMessageEventStream {
        self.runtime.stream(model, context, options)
    }

    /// Stream with provider-neutral options and request-time authentication.
    pub fn stream_simple(
        &self,
        model: Model,
        context: Context,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantMessageEventStream {
        self.runtime.stream_simple(model, context, options)
    }

    pub async fn complete(&self, model: Model, context: Context, options: Option<ModelsRequestOptions>) -> AssistantMessage {
        self.runtime.complete(model, context, options).await
    }

    /// Every known model of a type (chat, image, classifier), optionally for one provider.
    pub fn get_models_of_type(&self, r#type: ModelType, provider: Option<&str>) -> Vec<AnyModel> {
        self.runtime.get_models_of_type(r#type, provider)
    }

    /// Models of a type whose provider has working credentials.
    pub async fn get_available_of_type(
        &self,
        r#type: ModelType,
        provider: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> anyhow::Result<Vec<AnyModel>> {
        self.runtime.get_available_of_type(r#type, provider, options).await
    }

    pub fn get_model_of_type(&self, r#type: ModelType, provider: &str, model_id: &str) -> Option<AnyModel> {
        self.runtime.get_model_of_type(r#type, provider, model_id)
    }

    /// Classify structured state with request-time authentication. Never rejects.
    pub async fn classify(
        &self,
        model: ClassifierModel,
        context: ClassifierContext,
        options: Option<ModelsRequestOptions>,
    ) -> ClassifierResult {
        self.runtime.classify(model, context, options).await
    }

    /// Generate images with request-time authentication. Never rejects.
    pub async fn generate_images(
        &self,
        model: ImageModel,
        context: ImagesContext,
        options: Option<ModelsRequestOptions>,
    ) -> AssistantImages {
        self.runtime.generate_images(model, context, options).await
    }

    pub fn get_provider_display_name(&self, provider: &str) -> String {
        self.runtime
            .get_provider(provider)
            .map(|p| p.name().to_string())
            .unwrap_or_else(|| provider.to_string())
    }

    pub async fn get_provider_auth(&self, provider: &str) -> anyhow::Result<Option<AuthResult>> {
        self.runtime
            .get_auth(&ModelRuntimeGetAuthProviderOrModel::String(provider.to_string()), None)
            .await
    }

    pub async fn get_api_key_for_provider(&self, provider: &str) -> Option<String> {
        match self.get_provider_auth(provider).await {
            Ok(auth) => auth.and_then(|auth| auth.auth.api_key),
            Err(_) => None,
        }
    }

    pub fn is_using_o_auth(&self, model: &Model) -> bool {
        self.runtime.is_using_o_auth(&model.provider)
    }

    pub fn register_provider(
        &self,
        provider_or_name: &ModelRegistryRegisterProviderProviderOrName,
        config: Option<&ProviderConfigInput>,
    ) -> anyhow::Result<()> {
        match provider_or_name {
            ModelRegistryRegisterProviderProviderOrName::String(name) => {
                let Some(config) = config else {
                    anyhow::bail!("Provider config is required when registering by name");
                };
                self.runtime.register_provider(name, config)
            }
            ModelRegistryRegisterProviderProviderOrName::Arc(provider) => {
                self.runtime.register_native_provider(provider.clone())
            }
        }
    }

    pub fn unregister_provider(&self, provider_name: &str) {
        self.runtime.unregister_provider(provider_name);
    }

    pub fn register_virtual_model(&self, definition: &VirtualModelDefinition) -> anyhow::Result<()> {
        self.runtime.register_virtual_model(definition)
    }

    pub fn unregister_virtual_model(&self, provider_name: &str, id: &str) {
        self.runtime.unregister_virtual_model(provider_name, id);
    }

    pub fn get_registered_provider_config(&self, provider_name: &str) -> Option<ProviderConfigInput> {
        self.runtime.get_registered_provider_config(provider_name)
    }

    /// [`Self::get_registered_provider_config`] plus a revision that changes with every
    /// registration (JS extensions get the same config object until the next registration).
    pub fn get_registered_provider_config_revision(&self, provider_name: &str) -> Option<(u64, ProviderConfigInput)> {
        self.runtime.get_registered_provider_config_revision(provider_name)
    }

    pub fn get_registered_native_provider(&self, provider_name: &str) -> Option<Arc<dyn Provider>> {
        self.runtime.get_registered_native_provider(provider_name)
    }

    pub fn get_registered_provider_ids(&self) -> Vec<String> {
        self.runtime.get_registered_provider_ids()
    }
}
