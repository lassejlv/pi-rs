//! Radius gateway configuration (port of `providers/radius-config.ts`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::types::OAuthCredential;
use crate::types::{Model, ModelCost, ModelInputModality, ThinkingLevelMap};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInputModality>,
    pub cost: ModelCost,
    pub context_window: f64,
    pub max_tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

fn sanitize_radius_gateway_config(config: &Value) -> Option<RadiusGatewayConfig> {
    let object = config.as_object()?;
    let base_url = object.get("baseUrl")?.as_str()?.to_string();
    let models = object.get("models")?.as_array()?;
    Some(RadiusGatewayConfig {
        base_url,
        models: models.iter().filter_map(|m| serde_json::from_value::<RadiusGatewayModel>(m.clone()).ok()).collect(),
    })
}

/// Add `https://` when missing and strip trailing slashes.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let lower = value.to_lowercase();
    let with_scheme =
        if lower.starts_with("http://") || lower.starts_with("https://") { value.to_string() } else { format!("https://{value}") };
    with_scheme.trim_end_matches('/').to_string()
}

pub fn get_radius_credential_config(credential: Option<&OAuthCredential>) -> Option<RadiusGatewayConfig> {
    sanitize_radius_gateway_config(credential?.extra.get("gatewayConfig")?)
}

pub fn get_radius_models_from_config(provider_id: &str, config: &RadiusGatewayConfig) -> Vec<Model> {
    config
        .models
        .iter()
        .map(|m| Model {
            id: m.id.clone(),
            name: m.name.clone(),
            api: "pi-messages".into(),
            provider: provider_id.into(),
            base_url: config.base_url.clone(),
            reasoning: m.reasoning,
            thinking_level_map: m.thinking_level_map.clone(),
            input: m.input.clone(),
            cost: m.cost.clone(),
            context_window: m.context_window.max(0.0) as u64,
            max_tokens: m.max_tokens.max(0.0) as u64,
            ..Default::default()
        })
        .collect()
}

pub fn get_radius_models(provider_id: &str, credential: Option<&OAuthCredential>) -> Vec<Model> {
    get_radius_credential_config(credential).map(|c| get_radius_models_from_config(provider_id, &c)).unwrap_or_default()
}

fn truncate_http_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() > 512 { format!("{}…", trimmed.chars().take(512).collect::<String>()) } else { trimmed.to_string() }
}

/// Load `/v1/config` from a Radius gateway.
pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<RadiusGatewayConfig> {
    let url = url::Url::parse(gateway)?.join("/v1/config")?;
    let client = crate::utils::http::client_for(url.as_str(), None)?;
    let mut request = client.get(url.clone()).header("accept", "application/json");
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let send = request.send();
    let response = match signal {
        Some(signal) => tokio::select! {
            r = send => r?,
            _ = signal.cancelled() => anyhow::bail!(crate::utils::abort::SignalAbortError),
        },
        None => send.await?,
    };
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("Could not load Radius config from {gateway}: {}: {}", status.as_u16(), truncate_http_body(&text));
    }
    let body: Value = response.json().await?;
    sanitize_radius_gateway_config(&body).ok_or_else(|| anyhow::anyhow!("Invalid Radius config from {gateway}"))
}
