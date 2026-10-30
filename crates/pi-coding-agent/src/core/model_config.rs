//! Port of `packages/coding-agent/src/core/model-config.ts`.
//!
//! Immutable, credential-blind models.json snapshot.
//!
//! The JSON Schema is the generated `schemas/models.schema.json` artifact of the TS package
//! (TypeBox `ModelsConfigSchema` with the pi-ai model/compat schemas as `$defs`), embedded here and
//! checked with pi-ai's TypeBox-compatible validator.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::LazyLock;

use indexmap::IndexMap;
use pi_ai::providers::model_schema::{
    ModelCost, ModelCostTier, ModelInputLimits, ModelInputModality, ModelPromptCache,
    ModelThinkingLevel, ThinkingLevelMap,
};
use pi_ai::types::SamplingParamsByThinkingLevel;
use pi_ai::utils::validation::{SchemaValidator, ValidationError};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::utils::json::strip_json_comments;
use crate::utils::paths::normalize_path;
use crate::utils::text::strip_bom;

/// The generated models.json JSON Schema artifact.
pub const MODELS_SCHEMA_ARTIFACT: pi_embed::Embedded = pi_embed::include_str!("schemas/models.schema.json");

pub static MODELS_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(MODELS_SCHEMA_ARTIFACT.get()).expect("valid embedded models schema")
});

/// Replace `$ref`s into the root `$defs` with the referenced schema (siblings win).
fn inline_refs(schema: &Value, defs: &Map<String, Value>) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                && let Some(name) = reference.strip_prefix("#/$defs/")
                && let Some(Value::Object(target)) = defs.get(name)
            {
                for (key, value) in target {
                    out.insert(key.clone(), inline_refs(value, defs));
                }
            }
            for (key, value) in map {
                if key == "$ref" && !out.is_empty() {
                    continue;
                }
                out.insert(key.clone(), inline_refs(value, defs));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| inline_refs(v, defs)).collect()),
        other => other.clone(),
    }
}

/// Entry schemas rendered into docs/models-configuration.md.
pub static MODELS_JSON_SCHEMAS: LazyLock<Value> = LazyLock::new(|| {
    let root = &*MODELS_CONFIG_SCHEMA;
    let defs = root
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let provider = root
        .pointer("/properties/providers/patternProperties/^.*$")
        .cloned()
        .unwrap_or(Value::Null);
    let model = provider
        .pointer("/properties/models/items")
        .cloned()
        .unwrap_or(Value::Null);
    let model_override = provider
        .pointer("/properties/modelOverrides/patternProperties/^.*$")
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::json!({
        "provider": inline_refs(&provider, &defs),
        "model": inline_refs(&model, &defs),
        "modelOverride": inline_refs(&model_override, &defs),
    })
});

/// Deserialize a per-thinking-level map, ignoring keys that are not thinking levels (the schema
/// allows additional properties).
fn deserialize_level_map<'de, D, T>(
    deserializer: D,
) -> Result<Option<BTreeMap<ModelThinkingLevel, T>>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw: Option<Map<String, Value>> = Option::deserialize(deserializer)?;
    let Some(raw) = raw else { return Ok(None) };
    let mut out = BTreeMap::new();
    for (key, value) in raw {
        let Some(level) = ModelThinkingLevel::parse(&key) else {
            continue;
        };
        let value = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        out.insert(level, value);
    }
    Ok(Some(out))
}

/// A custom model in `providers.<id>.models`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModel {
    /// Model ID sent to the API.
    pub id: String,
    /// Display name, also matched by --model patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Overrides the provider api.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Overrides the provider baseUrl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Whether the model supports thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_level_map"
    )]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<ModelInputModality>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    /// Context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    /// Maximum output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    /// Fields merged into every request body. Applies only to OpenAI-compatible APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    /// samplingParams per thinking level, merged over samplingParams.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_level_map"
    )]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    /// Request headers. Values resolve like apiKey.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Provider and model compatibility overrides (`ProviderCompat`, kept as JSON so unknown
    /// settings survive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
}

/// Partial cost of a model override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModelOverrideCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

/// A `providers.<id>.modelOverrides.<modelId>` entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_level_map"
    )]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<ModelInputModality>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelsJsonModelOverrideCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_level_map"
    )]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
}

/// A `providers.<id>` entry of models.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsJsonProvider {
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// API base URL. Also applies to the provider's built-in models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Literal key, "$NAME" environment interpolation, or "!command" output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// API for the provider's models, such as "openai-completions", "openai-responses", "anthropic-messages", or "google-generative-ai".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// OAuth login flow. Requires baseUrl. Always "radius".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    /// Request headers. Values resolve like apiKey.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Provider and model compatibility overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Map<String, Value>>,
    /// Whether to send the resolved API key as Authorization: Bearer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelsJsonModel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<IndexMap<String, ModelsJsonModelOverride>>,
}

fn format_validation_path(error: &ValidationError) -> String {
    let base_path = error
        .instance_path
        .strip_prefix('/')
        .unwrap_or(&error.instance_path)
        .replace('/', ".");
    if error.keyword == "required"
        && let Some(required) = error.required_properties.first()
    {
        return if base_path.is_empty() {
            required.clone()
        } else {
            format!("{base_path}.{required}")
        };
    }
    if base_path.is_empty() {
        "root".to_string()
    } else {
        base_path
    }
}

/// One immutable load of models.json.
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    providers: IndexMap<String, ModelsJsonProvider>,
    error: Option<String>,
}

impl ModelConfig {
    pub fn new(providers: IndexMap<String, ModelsJsonProvider>, error: Option<&str>) -> Self {
        Self {
            providers,
            error: error.map(str::to_string),
        }
    }

    /// Parse models.json content (the part of `load` after reading the file).
    pub fn from_content(content: &str, path: &str) -> ModelConfig {
        let parsed: Value = match crate::utils::js_json::js_json_parse(&strip_json_comments(&strip_bom(content))) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ModelConfig::new(
                    IndexMap::new(),
                    Some(&format!("Failed to parse models.json: {error}\n\nFile: {path}")),
                );
            }
        };

        let validator = SchemaValidator::new(&MODELS_CONFIG_SCHEMA);
        if !validator.check(&parsed) {
            let errors = validator
                .errors(&parsed)
                .iter()
                .map(|error| format!("  - {}: {}", format_validation_path(error), error.message))
                .collect::<Vec<_>>()
                .join("\n");
            let errors = if errors.is_empty() {
                "Unknown schema error".to_string()
            } else {
                errors
            };
            return ModelConfig::new(
                IndexMap::new(),
                Some(&format!("Invalid models.json schema:\n{errors}\n\nFile: {path}")),
            );
        }

        let mut providers = IndexMap::new();
        if let Some(entries) = parsed.get("providers").and_then(Value::as_object) {
            for (provider_id, provider) in entries {
                match serde_json::from_value::<ModelsJsonProvider>(provider.clone()) {
                    Ok(provider) => {
                        providers.insert(provider_id.clone(), provider);
                    }
                    Err(error) => {
                        return ModelConfig::new(
                            IndexMap::new(),
                            Some(&format!(
                                "Invalid models.json schema:\n  - providers.{provider_id}: {error}\n\nFile: {path}"
                            )),
                        );
                    }
                }
            }
        }
        ModelConfig::new(providers, None)
    }

    pub async fn load(models_json_path: Option<&str>) -> Arc<ModelConfig> {
        let Some(models_json_path) = models_json_path else {
            return Arc::new(ModelConfig::default());
        };
        let path = normalize_path(models_json_path, None);
        let content = match tokio::fs::read(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Arc::new(ModelConfig::default());
            }
            Err(error) => {
                return Arc::new(ModelConfig::new(
                    IndexMap::new(),
                    Some(&format!("Failed to load models.json: {error}\n\nFile: {path}")),
                ));
            }
        };
        Arc::new(ModelConfig::from_content(&content, &path))
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<&ModelsJsonProvider> {
        self.providers.get(provider_id)
    }

    pub fn get_provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    pub fn get_error(&self) -> Option<String> {
        self.error.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(value: Value) -> ModelConfig {
        ModelConfig::from_content(&value.to_string(), "/tmp/models.json")
    }

    #[test]
    fn accepts_custom_compat_and_rejects_malformed_known_fields() {
        let compat = json!({ "customOption": "value", "supportsStore": true, "supportsAdditionalTools": true });
        let config = load(json!({ "providers": { "demo": { "api": "custom-api", "compat": compat } } }));
        assert_eq!(config.get_error(), None);
        assert_eq!(
            Value::Object(config.get_provider("demo").unwrap().compat.clone().unwrap()),
            compat
        );

        let config = load(json!({ "providers": { "demo": { "compat": { "supportsLongCacheRetention": "yes" } } } }));
        assert!(config.get_error().unwrap().contains("Invalid models.json schema"));
    }

    #[test]
    fn validates_schema_field() {
        let config = load(json!({
            "$schema": "https://pi.dev/schemas/models.schema.json",
            "providers": { "demo": { "baseUrl": "http://localhost:8080/v1", "models": [{ "id": "demo" }] } }
        }));
        assert_eq!(config.get_error(), None);
        assert_eq!(config.get_provider("demo").unwrap().models.as_ref().unwrap()[0].id, "demo");

        let config = load(json!({ "providers": { "demo": { "models": [{ "id": "" }] } } }));
        assert!(config.get_error().unwrap().contains("Invalid models.json schema"));

        let config = load(json!({ "$schema": 42, "providers": {} }));
        assert!(config.get_error().unwrap().contains("$schema"));
    }

    #[test]
    fn reports_parse_errors_and_strips_comments() {
        let config = ModelConfig::from_content("{ not json", "/x/models.json");
        let error = config.get_error().unwrap();
        assert!(error.starts_with("Failed to parse models.json"));
        assert!(error.ends_with("File: /x/models.json"));

        let config = ModelConfig::from_content(
            "{\n // comment\n \"providers\": { \"a\": { \"baseUrl\": \"http://x\", } },\n}",
            "/x/models.json",
        );
        assert_eq!(config.get_error(), None);
        assert_eq!(config.get_provider_ids(), vec!["a".to_string()]);
    }

    #[test]
    fn requires_providers() {
        let config = load(json!({}));
        let error = config.get_error().unwrap();
        assert!(error.contains("  - providers: "), "{error}");
    }

    #[test]
    fn entry_schemas_are_inlined() {
        let schemas = &*MODELS_JSON_SCHEMAS;
        assert_eq!(schemas["model"]["required"], json!(["id"]));
        assert!(schemas["provider"]["properties"]["compat"]["properties"].is_object());
        assert!(!schemas.to_string().contains("$ref"));
    }
}
