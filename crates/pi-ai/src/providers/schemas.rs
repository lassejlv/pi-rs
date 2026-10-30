//! JSON Schemas of the model metadata and compatibility settings (the TypeBox schemas in
//! `providers/model-schema.ts` and `providers/compat-schema.ts`), for validating
//! `models.json`-style configuration. Generated from the TypeScript definitions into
//! `data/schemas.json`.

use once_cell::sync::Lazy;
use serde_json::{Map, Value};

static SCHEMAS: Lazy<Map<String, Value>> = Lazy::new(|| {
    serde_json::from_str(include_str!("../../data/schemas.json")).expect("valid embedded schemas")
});

/// A schema by its TypeScript export name, e.g. `"ProviderCompatSchema"`.
pub fn schema(name: &str) -> Option<&'static Value> {
    SCHEMAS.get(name)
}

fn get(name: &str) -> &'static Value {
    SCHEMAS.get(name).unwrap_or_else(|| panic!("schema {name} is embedded"))
}

pub fn thinking_level_schema() -> &'static Value {
    get("ThinkingLevelSchema")
}
pub fn model_thinking_level_schema() -> &'static Value {
    get("ModelThinkingLevelSchema")
}
pub fn model_input_modality_schema() -> &'static Value {
    get("ModelInputModalitySchema")
}
pub fn cache_retention_schema() -> &'static Value {
    get("CacheRetentionSchema")
}
pub fn thinking_level_map_schema() -> &'static Value {
    get("ThinkingLevelMapSchema")
}
pub fn model_prompt_cache_schema() -> &'static Value {
    get("ModelPromptCacheSchema")
}
pub fn model_cost_rates_schema() -> &'static Value {
    get("ModelCostRatesSchema")
}
pub fn model_cost_tier_schema() -> &'static Value {
    get("ModelCostTierSchema")
}
pub fn model_cost_schema() -> &'static Value {
    get("ModelCostSchema")
}
pub fn model_image_resize_options_schema() -> &'static Value {
    get("ModelImageResizeOptionsSchema")
}
pub fn model_image_input_limits_schema() -> &'static Value {
    get("ModelImageInputLimitsSchema")
}
pub fn model_input_limits_schema() -> &'static Value {
    get("ModelInputLimitsSchema")
}
pub fn session_affinity_format_schema() -> &'static Value {
    get("SessionAffinityFormatSchema")
}
pub fn thinking_token_budget_field_schema() -> &'static Value {
    get("ThinkingTokenBudgetFieldSchema")
}
pub fn chat_template_kwarg_value_schema() -> &'static Value {
    get("ChatTemplateKwargValueSchema")
}
pub fn open_router_routing_schema() -> &'static Value {
    get("OpenRouterRoutingSchema")
}
pub fn vercel_gateway_routing_schema() -> &'static Value {
    get("VercelGatewayRoutingSchema")
}
pub fn anthropic_allowed_fallback_model_schema() -> &'static Value {
    get("AnthropicAllowedFallbackModelSchema")
}
pub fn openai_completions_compat_schema() -> &'static Value {
    get("OpenAICompletionsCompatSchema")
}
pub fn openai_responses_compat_schema() -> &'static Value {
    get("OpenAIResponsesCompatSchema")
}
pub fn anthropic_messages_compat_schema() -> &'static Value {
    get("AnthropicMessagesCompatSchema")
}
pub fn bedrock_compat_schema() -> &'static Value {
    get("BedrockCompatSchema")
}
pub fn mistral_conversations_compat_schema() -> &'static Value {
    get("MistralConversationsCompatSchema")
}
pub fn system_one_compat_schema() -> &'static Value {
    get("SystemOneCompatSchema")
}
/// Provider-level compat superset (`ProviderCompatSchema`).
pub fn provider_compat_schema() -> &'static Value {
    get("ProviderCompatSchema")
}

/// Compat schema for an API whose models accept compat (`CompatSchemasByApi`).
pub fn compat_schema_for_api(api: &str) -> Option<&'static Value> {
    let name = SCHEMAS.get("CompatSchemasByApi")?.get(api)?.as_str()?;
    SCHEMAS.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::validation::check_schema;
    use serde_json::json;

    #[test]
    fn schemas_validate_catalog_shapes() {
        assert!(check_schema(model_cost_schema(), &json!({"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0})));
        assert!(!check_schema(model_cost_schema(), &json!({"input": 1})));
        assert!(check_schema(provider_compat_schema(), &json!({"supportsStore": true, "unknownKey": 1})));
        assert!(!check_schema(provider_compat_schema(), &json!({"supportsStore": "yes"})));
        assert!(compat_schema_for_api("anthropic-messages").is_some());
        assert!(compat_schema_for_api("unknown").is_none());
        assert!(check_schema(thinking_level_map_schema(), &json!({"off": null, "high": "high"})));
    }
}
