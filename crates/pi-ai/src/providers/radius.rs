//! Radius gateway provider (port of `providers/radius.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use super::radius_config::{
    DEFAULT_RADIUS_GATEWAY, get_radius_models, get_radius_models_from_config, load_radius_gateway_config,
    normalize_radius_gateway_url,
};
use crate::api::registry::pi_messages_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::oauth_load::{OAuthFlowArgs, lazy_builtin_oauth};
use crate::auth::types::{Credential, ProviderAuth};
use crate::models::{ModelsPublication, Provider, ProviderStreams, RefreshModelsContext};
use crate::models_generated::CATALOG;
use crate::models_store::ModelsStoreEntry;
use crate::types::{AnyModel, AssistantMessageEventStream, Model, StreamOptions, TranscriptContext};

#[derive(Debug, Clone, Default)]
pub struct RadiusProviderOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub gateway: Option<String>,
}

struct RadiusProvider {
    id: String,
    name: String,
    gateway: String,
    auth: ProviderAuth,
    baseline: Vec<Model>,
    dynamic: Arc<RwLock<Option<Vec<Model>>>>,
    streams: Arc<dyn ProviderStreams>,
}

#[async_trait]
impl Provider for RadiusProvider {
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
        self.dynamic.read().clone().unwrap_or_else(|| self.baseline.clone())
    }
    fn supports_refresh(&self) -> bool {
        true
    }
    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        let stored = context.stored.clone();
        if let Some(stored) = &stored {
            let restored: Vec<Model> = stored
                .models
                .iter()
                .filter(|m| m.provider() == self.id)
                .filter_map(|m| m.as_chat().cloned())
                .collect();
            let dynamic = self.dynamic.clone();
            if !(context.publish)(ModelsPublication { persist: None, update: Some(Box::new(move || *dynamic.write() = Some(restored))) })
                .await?
            {
                return Ok(());
            }
        }
        if stored.is_none()
            && let Some(Credential::OAuth(oauth)) = &context.credential {
                let legacy = get_radius_models(&self.id, Some(oauth));
                if !legacy.is_empty() {
                    let dynamic = self.dynamic.clone();
                    let persisted = legacy.iter().cloned().map(AnyModel::Chat).collect();
                    if !(context.publish)(ModelsPublication {
                        persist: Some(Some(ModelsStoreEntry {
                            models: persisted,
                            checked_at: Some(crate::utils::now_ms()),
                            ..Default::default()
                        })),
                        update: Some(Box::new(move || *dynamic.write() = Some(legacy))),
                    })
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
        if !context.allow_network || context.signal.is_cancelled() {
            return Ok(());
        }
        let api_key = match &context.credential {
            Some(Credential::OAuth(oauth)) => Some(oauth.access.clone()),
            Some(Credential::ApiKey(key)) => key.key.clone(),
            None => None,
        };
        let config = crate::utils::wasm::assert_send(load_radius_gateway_config(&self.gateway, api_key.as_deref(), Some(&context.signal))).await?;
        if context.signal.is_cancelled() {
            return Ok(());
        }
        let refreshed = get_radius_models_from_config(&self.id, &config);
        let dynamic = self.dynamic.clone();
        let persisted = refreshed.iter().cloned().map(AnyModel::Chat).collect();
        (context.publish)(ModelsPublication {
            persist: Some(Some(ModelsStoreEntry {
                models: persisted,
                checked_at: Some(crate::utils::now_ms()),
                ..Default::default()
            })),
            update: Some(Box::new(move || *dynamic.write() = Some(refreshed))),
        })
        .await?;
        Ok(())
    }
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.streams.stream(model, context, options)
    }
    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.streams.stream_simple(model, context, options)
    }
}

/// Radius gateway provider with a persisted, dynamically refreshed catalog.
pub fn radius_provider(options: RadiusProviderOptions) -> Arc<dyn Provider> {
    let id = options.id.unwrap_or_else(|| "radius".into());
    let name = options.name.unwrap_or_else(|| "Radius".into());
    let gateway = normalize_radius_gateway_url(options.gateway.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY));
    let baseline = if gateway == normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY) {
        CATALOG
            .chat
            .get("radius")
            .map(|models| models.values().map(|m| Model { provider: id.clone(), ..m.clone() }).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let auth = ProviderAuth {
        api_key: Some(env_api_key_auth("Radius API key", &["RADIUS_API_KEY"])),
        oauth: Some(lazy_builtin_oauth(
            &name,
            None,
            None,
            "radius",
            OAuthFlowArgs { name: Some(name.clone()), gateway: Some(gateway.clone()) },
        )),
    };
    Arc::new(RadiusProvider {
        id,
        name,
        gateway,
        auth,
        baseline,
        dynamic: Arc::new(RwLock::new(None)),
        streams: pi_messages_api(),
    })
}
