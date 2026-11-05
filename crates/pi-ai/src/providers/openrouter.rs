//! OpenRouter provider (port of `providers/openrouter.ts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::Value;

use super::builtin::{catalog_chat_models, catalog_classifier_models, catalog_image_models};
use super::openrouter_api::{OpenRouterModel, open_router_cost};
use crate::api::registry::{anthropic_messages_api, builtin_classifier_api, builtin_images_api, openai_completions_api};
use crate::auth::helpers::env_api_key_auth;
use crate::auth::oauth_load::{OAuthFlowArgs, lazy_builtin_oauth};
use crate::auth::types::{Credential, ProviderAuth};
use crate::models::{CreateProviderOptions, Provider, ProviderApi, RefreshModelsContext, create_provider};
use crate::types::AnyModel;

const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_USER_MODELS_TIMEOUT_MS: u64 = 15_000;

/// Restrict an OpenRouter catalog to the chat models the key may use, with the key's limits
/// and prices from `GET /models/user`.
pub async fn personalize_open_router_catalog(catalog: Vec<AnyModel>, context: &RefreshModelsContext) -> anyhow::Result<Vec<AnyModel>> {
    let key = match &context.credential {
        Some(Credential::OAuth(oauth)) => Some(oauth.access.clone()),
        Some(Credential::ApiKey(api_key)) => api_key.key.clone(),
        None => None,
    };
    let Some(key) = key.filter(|k| !k.is_empty()) else { return Ok(catalog) };
    let base = context.base_url.clone().unwrap_or_else(|| OPENROUTER_BASE_URL.to_string());
    let url = format!("{}/models/user", base.trim_end_matches('/'));
    let client = crate::utils::http::client_for(&url, None)?;
    let request = client
        .get(&url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .timeout(Duration::from_millis(OPENROUTER_USER_MODELS_TIMEOUT_MS))
        .send();
    let response = tokio::select! {
        r = request => r?,
        _ = context.signal.cancelled() => anyhow::bail!(crate::utils::abort::SignalAbortError),
    };
    let status = response.status().as_u16();
    if matches!(status, 401 | 403 | 404) {
        return Ok(catalog);
    }
    if !response.status().is_success() {
        anyhow::bail!("OpenRouter model list request failed: {status}");
    }
    let body: Value = response.json().await?;
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        anyhow::bail!("Invalid OpenRouter model list response");
    };
    let mut allowed: HashMap<String, OpenRouterModel> = HashMap::new();
    for entry in data {
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            let model: OpenRouterModel =
                serde_json::from_value(entry.clone()).unwrap_or(OpenRouterModel { id: id.to_string(), ..Default::default() });
            allowed.insert(id.to_string(), model);
        }
    }
    let positive = |v: Option<f64>| v.filter(|x| *x > 0.0);
    Ok(catalog
        .into_iter()
        .filter_map(|model| match model {
            AnyModel::Chat(mut chat) => {
                let entry = allowed.get(&chat.id)?;
                let top = entry.top_provider.as_ref();
                chat.context_window = positive(top.and_then(|t| t.context_length))
                    .or(positive(entry.context_length))
                    .map(|v| v as u64)
                    .unwrap_or(chat.context_window);
                chat.max_tokens =
                    positive(top.and_then(|t| t.max_completion_tokens)).map(|v| v as u64).unwrap_or(chat.max_tokens);
                if entry.pricing.is_some() {
                    chat.cost = open_router_cost(entry.pricing.as_ref());
                }
                Some(AnyModel::Chat(chat))
            }
            other => Some(other),
        })
        .collect())
}

pub fn openrouter_provider() -> Arc<dyn Provider> {
    let mut apis = IndexMap::new();
    apis.insert("anthropic-messages".to_string(), anthropic_messages_api());
    apis.insert("openai-completions".to_string(), openai_completions_api());
    let mut images = IndexMap::new();
    images.insert("openrouter-images".to_string(), builtin_images_api("openrouter-images"));
    let mut classifiers = IndexMap::new();
    classifiers.insert("typesafe-system-one".to_string(), builtin_classifier_api("typesafe-system-one"));
    let mut models = catalog_chat_models("openrouter");
    models.extend(catalog_image_models("openrouter"));
    models.extend(catalog_classifier_models("openrouter"));
    create_provider(CreateProviderOptions {
        id: "openrouter".into(),
        name: Some("OpenRouter".into()),
        base_url: Some(OPENROUTER_BASE_URL.into()),
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth("OpenRouter API key", &["OPENROUTER_API_KEY"])),
            oauth: Some(lazy_builtin_oauth("OpenRouter OAuth", None, Some("Sign in with OpenRouter"), "openrouter", OAuthFlowArgs::default())),
        },
        models,
        api: Some(ProviderApi::ByApi(apis)),
        images,
        classifiers,
        ..Default::default()
    })
    .expect("openrouter provider is valid")
}
