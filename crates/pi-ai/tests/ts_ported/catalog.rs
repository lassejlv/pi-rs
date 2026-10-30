//! Catalog metadata and catalog-driven payload tests: `supports-xhigh.test.ts`,
//! `xiaomi-models.test.ts`, `zai-coding-plan-models.test.ts`, `qwen-token-plan-models.test.ts`,
//! `openrouter-cache-control-models.test.ts`, `model-catalog-types.test.ts`,
//! `openrouter-reasoning-options.test.ts`, `fireworks-model-generation.test.ts`,
//! `model-cost-tiers.test.ts` and `lax-message-content.test.ts`.

use pi_ai::env_api_keys::find_env_keys;
use pi_ai::providers::all::{get_builtin_model, get_builtin_models};
use pi_ai::providers::openrouter_api::{OpenRouterPricing, open_router_cost};
use pi_ai::*;
use serde_json::{Value, json};

use crate::support::*;

fn builtin(provider: &str, id: &str) -> Model {
    get_builtin_model(provider, id).unwrap_or_else(|| panic!("{provider}/{id}"))
}

fn levels(model: &Model) -> Vec<String> {
    get_supported_thinking_levels(model).into_iter().map(|l| serde_json::to_value(l).unwrap().as_str().unwrap().to_string()).collect()
}

fn has(model: &Model, level: &str) -> bool {
    levels(model).iter().any(|l| l == level)
}

/// `toMatchObject`: every key of `expected` matches recursively.
fn assert_matches(actual: &Value, expected: &Value, path: &str) {
    match expected {
        Value::Object(map) => {
            for (key, value) in map {
                assert_matches(&actual[key], value, &format!("{path}.{key}"));
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

// ---------------------------------------------------------------------------
// Port of supports-xhigh.test.ts
// ---------------------------------------------------------------------------

#[test]
fn anthropic_thinking_levels() {
    let opus_46 = builtin("anthropic", "claude-opus-4-6");
    assert!(has(&opus_46, "max") && !has(&opus_46, "xhigh"));
    for id in ["claude-opus-4-8", "claude-opus-5", "claude-sonnet-5"] {
        let model = builtin("anthropic", id);
        assert!(has(&model, "xhigh") && has(&model, "max"), "{id}");
    }
    let sonnet_46 = builtin("anthropic", "claude-sonnet-4-6");
    assert!(has(&sonnet_46, "max") && !has(&sonnet_46, "xhigh"));
    let fable = builtin("anthropic", "claude-fable-5");
    assert!(has(&fable, "xhigh") && has(&fable, "max") && !has(&fable, "off"));
    let sonnet_45 = builtin("anthropic", "claude-sonnet-4-5");
    assert!(!has(&sonnet_45, "xhigh") && !has(&sonnet_45, "max"));

    let opus_55 = builtin("anthropic", "claude-opus-5-5");
    assert_matches(
        &serde_json::to_value(&opus_55).unwrap(),
        &json!({"cost": {"input": 4, "output": 20, "cacheRead": 0.2, "cacheWrite": 5}, "contextWindow": 1000000, "maxTokens": 128000,
            "compat": {"forceAdaptiveThinking": true, "supportsMidConvoEffort": true, "supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true}}),
        "opus-5-5",
    );
    assert_eq!(levels(&opus_55), ["low", "medium", "high", "xhigh", "max"]);
    let sonnet_55 = builtin("anthropic", "claude-sonnet-5-5");
    assert_matches(
        &serde_json::to_value(&sonnet_55).unwrap(),
        &json!({"cost": {"input": 2, "output": 10, "cacheRead": 0.1, "cacheWrite": 2.5}, "contextWindow": 1000000, "maxTokens": 128000,
            "compat": {"forceAdaptiveThinking": true, "supportsMidConvoEffort": true, "supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true, "supportsTemperature": false}}),
        "sonnet-5-5",
    );
    assert_eq!(levels(&sonnet_55), ["low", "medium", "high", "xhigh", "max"]);
}

#[test]
fn openai_and_codex_thinking_levels_and_metadata() {
    for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-6.1-sol"] {
        assert!(has(&builtin("openai-codex", id), "xhigh"), "{id}");
    }
    for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-6-sol", "gpt-6-luna"] {
        assert_eq!(levels(&builtin("openai", id)), ["off", "low", "medium", "high", "xhigh", "max"], "{id}");
    }
    // OpenAI and Codex reject reasoning.effort "none" for GPT-6.1 Sol.
    for (provider, expected) in [
        ("openai", vec!["low", "medium", "high", "xhigh", "max"]),
        ("azure", vec!["low", "medium", "high", "xhigh", "max"]),
        ("openai-codex", vec!["minimal", "low", "medium", "high", "xhigh", "max"]),
    ] {
        let model = builtin(provider, "gpt-6.1-sol");
        assert_eq!(levels(&model), expected, "{provider}");
        let map = serde_json::to_value(&model.thinking_level_map).unwrap();
        assert!(map.get("off").is_some_and(Value::is_null), "{provider}: {map}");
    }
    for (id, cost, codex_context) in [
        ("gpt-6-sol", json!({"input": 2, "output": 10, "cacheRead": 0.2, "cacheWrite": 2.5}), 272000),
        ("gpt-6-luna", json!({"input": 0.1, "output": 0.5, "cacheRead": 0.01, "cacheWrite": 0.125}), 272000),
        // Codex serves GPT-6.1 Sol with a 500K default context window (#10840)
        ("gpt-6.1-sol", json!({"input": 2, "output": 10, "cacheRead": 0.1, "cacheWrite": 2.5}), 500000),
    ] {
        for provider in ["openai", "openai-codex"] {
            let model = serde_json::to_value(builtin(provider, id)).unwrap();
            let f = |k: &str| cost[k].as_f64().unwrap();
            assert_matches(&model["cost"], &cost, &format!("{provider}/{id}.cost"));
            let tier = &model["cost"]["tiers"];
            assert_eq!(tier.as_array().unwrap().len(), 1, "{provider}/{id}");
            assert_eq!(tier[0]["inputTokensAbove"], 272000);
            for (key, factor) in [("input", 2.0), ("output", 1.5), ("cacheRead", 2.0), ("cacheWrite", 2.0)] {
                assert!((tier[0][key].as_f64().unwrap() - f(key) * factor).abs() < 1e-9, "{provider}/{id} tier {key}");
            }
            assert_eq!(model["input"], json!(["text", "image"]));
            assert_eq!(model["contextWindow"], if provider == "openai-codex" { codex_context } else { 272000 });
            assert_eq!(model["maxTokens"], 128000);
            assert_matches(
                &model["compat"],
                &json!({"supportsAdditionalTools": true, "supportsMidConvoSystemMessages": true, "supportsOpenAIGrammarTools": true, "supportsToolSearch": true}),
                &format!("{provider}/{id}.compat"),
            );
        }
    }
    assert_eq!(levels(&builtin("openai", "gpt-5.5-pro")), ["medium", "high", "xhigh"]);
    assert_eq!(levels(&builtin("openrouter", "openai/gpt-5.5-pro")), ["medium", "high", "xhigh"]);
}

#[test]
fn other_provider_thinking_levels() {
    assert_eq!(levels(&builtin("deepseek", "deepseek-flash")), ["off", "low", "high", "max"]);
    assert_eq!(levels(&builtin("openrouter", "deepseek/deepseek-v4.1-flash")), ["off", "low", "high", "max"]);
    assert_eq!(levels(&builtin("opencode-go", "deepseek-v4.1-flash")), ["low", "high", "max"]);
    for provider in ["moonshotai", "moonshotai-cn"] {
        assert_eq!(levels(&builtin(provider, "kimi-k2.7-code")), ["minimal", "low", "medium", "high"], "{provider}");
        assert_eq!(levels(&builtin(provider, "kimi-k3")), ["low", "high", "max"], "{provider}");
    }
    assert_eq!(levels(&builtin("kimi-coding", "k3")), ["low", "high", "max"]);
    assert!(!has(&builtin("kimi-coding", "kimi-for-coding-highspeed"), "off"));
    assert_eq!(levels(&builtin("opencode", "grok-build-0.1")), ["high"]);
    assert_eq!(levels(&builtin("openrouter", "deepseek/deepseek-v4-flash")), ["off", "high", "xhigh"]);
    let opus_46 = builtin("openrouter", "anthropic/claude-opus-4.6");
    assert!(has(&opus_46, "max") && !has(&opus_46, "xhigh"));
    let bedrock_opus = builtin("amazon-bedrock", "global.anthropic.claude-opus-5");
    assert!(has(&bedrock_opus, "xhigh") && has(&bedrock_opus, "max"));
    assert_eq!(levels(&builtin("xai", "grok-4.6")), ["low", "medium", "high", "xhigh"]);
    let bedrock_fable = builtin("amazon-bedrock", "global.anthropic.claude-fable-5");
    assert!(has(&bedrock_fable, "xhigh") && has(&bedrock_fable, "max") && !has(&bedrock_fable, "off"));
}

// ---------------------------------------------------------------------------
// Port of xiaomi-models.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn xiaomi_models_and_thinking_toggle() {
    for provider in ["xiaomi", "xiaomi-token-plan-cn", "xiaomi-token-plan-ams", "xiaomi-token-plan-sgp"] {
        let models = get_builtin_models(provider);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        for deprecated in ["mimo-v2-flash", "mimo-v2-omni", "mimo-v2-pro"] {
            assert!(!ids.contains(&deprecated), "{provider} {deprecated}");
        }
        for replacement in ["mimo-v2.6-flash", "mimo-v2.6-pro"] {
            assert!(ids.contains(&replacement), "{provider} {replacement}");
        }
        // #10818: MiMo Chat Completions only supports a thinking on/off toggle.
        let reasoning: Vec<&Model> = models.iter().filter(|m| m.reasoning).collect();
        assert!(!reasoning.is_empty());
        for model in reasoning {
            assert_eq!(levels(model), ["off", "high"], "{provider}/{}", model.id);
            let context = user_ctx("test");
            let enabled = simple_payload(model, &context, StreamOptions { reasoning: Some(ThinkingLevel::High), ..Default::default() }).await;
            assert_eq!(enabled["thinking"], json!({"type": "enabled"}));
            assert!(enabled.get("reasoning_effort").is_none());
            let disabled = simple_payload(model, &context, StreamOptions::default()).await;
            assert_eq!(disabled["thinking"], json!({"type": "disabled"}));
            assert!(disabled.get("reasoning_effort").is_none());
        }
    }
}

// ---------------------------------------------------------------------------
// Port of zai-coding-plan-models.test.ts
// ---------------------------------------------------------------------------

#[test]
fn zai_coding_plan_catalog() {
    assert_matches(
        &serde_json::to_value(builtin("zai-coding-cn", "glm-4.6v")).unwrap(),
        &json!({"id": "glm-4.6v", "provider": "zai-coding-cn", "api": "openai-completions", "baseUrl": "https://open.bigmodel.cn/api/coding/paas/v4",
            "reasoning": true, "input": ["text", "image"], "cost": {"input": 0.3, "output": 0.9, "cacheRead": 0.05, "cacheWrite": 0},
            "contextWindow": 128000, "maxTokens": 32768, "compat": {"maxTokensField": "max_tokens", "thinkingFormat": "zai", "zaiToolStream": true}}),
        "glm-4.6v",
    );
    let reference = json!({"input": 1.4, "output": 4.4, "cacheRead": 0.26, "cacheWrite": 0});
    assert_eq!(serde_json::to_value(builtin("zai", "glm-5.2").cost).unwrap(), reference);
    for provider in ["zai", "zai-coding-cn"] {
        assert_eq!(serde_json::to_value(builtin(provider, "glm-5.3").cost).unwrap(), reference);
    }
    let zero = json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0});
    assert_eq!(serde_json::to_value(builtin("zai", "glm-5.2-highspeed").cost).unwrap(), zero);
    for provider in ["zai", "zai-coding-cn"] {
        assert_eq!(serde_json::to_value(builtin(provider, "glm-5.3-highspeed").cost).unwrap(), zero);
    }
}

// ---------------------------------------------------------------------------
// Port of qwen-token-plan-models.test.ts
// ---------------------------------------------------------------------------

const QWEN_TEXT_MODELS: [&str; 16] = [
    "MiniMax-M2.5", "deepseek-v3.2", "deepseek-v4-flash", "deepseek-v4-pro", "glm-5", "glm-5.1", "glm-5.2", "kimi-k2.5", "kimi-k2.6",
    "kimi-k2.7-code", "qwen3.6-flash", "qwen3.6-plus", "qwen3.7-max", "qwen3.7-plus", "qwen3.8-flash", "qwen3.8-max",
];
const QWEN_INDIVIDUAL_TEXT_MODELS: [&str; 9] = [
    "deepseek-v4-flash-0731", "deepseek-v4-pro", "deepseek-v4-pro-0813", "glm-5.2", "qwen3.6-flash", "qwen3.7-max", "qwen3.7-plus", "qwen3.8-flash",
    "qwen3.8-max",
];
const QWEN_THINKING_MODELS: [&str; 15] = [
    "deepseek-v3.2", "deepseek-v4-flash", "deepseek-v4-pro", "glm-5", "glm-5.1", "glm-5.2", "kimi-k2.5", "kimi-k2.6", "kimi-k2.7-code",
    "qwen3.6-flash", "qwen3.6-plus", "qwen3.7-max", "qwen3.7-plus", "qwen3.8-flash", "qwen3.8-max",
];

fn qwen_cases(shared: &[&'static str], individual: &[&'static str]) -> Vec<(&'static str, &'static str)> {
    let mut cases: Vec<(&str, &str)> =
        ["qwen-token-plan", "qwen-token-plan-cn"].iter().flat_map(|p| shared.iter().map(move |m| (*p, *m))).collect();
    cases.extend(individual.iter().map(|m| ("qwen-token-plan-individual", *m)));
    cases
}

#[test]
fn qwen_token_plan_catalog() {
    let mut individual: Vec<String> = get_builtin_models("qwen-token-plan-individual").into_iter().map(|m| m.id).collect();
    individual.sort();
    let mut expected: Vec<String> = QWEN_INDIVIDUAL_TEXT_MODELS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(individual, expected);
    let env: ProviderEnv = [("QWEN_TOKEN_PLAN_API_KEY".to_string(), "test".to_string())].into_iter().collect();
    assert_eq!(find_env_keys("qwen-token-plan-individual", Some(&env)), Some(vec!["QWEN_TOKEN_PLAN_API_KEY".to_string()]));
    for provider in ["qwen-token-plan", "qwen-token-plan-cn"] {
        let ids: Vec<String> = get_builtin_models(provider).into_iter().map(|m| m.id).collect();
        for id in QWEN_TEXT_MODELS {
            assert!(ids.iter().any(|i| i == id), "{provider} should include {id}");
        }
        for id in ["qwen-image-2.0", "qwen-image-2.0-pro", "wan2.7-image", "wan2.7-image-pro"] {
            assert!(!ids.iter().any(|i| i == id), "{provider} should not include {id}");
        }
    }
    for provider in ["qwen-token-plan", "qwen-token-plan-cn", "qwen-token-plan-individual"] {
        assert!(get_builtin_model(provider, "qwen3.8-max-preview").is_none(), "{provider}");
    }
    let effort_cases = qwen_cases(
        &["deepseek-v4-flash", "deepseek-v4-pro", "glm-5", "glm-5.1", "glm-5.2"],
        &["deepseek-v4-flash-0731", "deepseek-v4-pro", "deepseek-v4-pro-0813", "glm-5.2"],
    );
    for (provider, id) in effort_cases {
        let map = serde_json::to_value(builtin(provider, id).thinking_level_map).unwrap();
        assert_matches(&map, &json!({"minimal": null, "low": null, "medium": null, "high": "high", "xhigh": null, "max": "max"}), &format!("{provider}/{id}"));
    }
    for provider in ["qwen-token-plan", "qwen-token-plan-cn", "qwen-token-plan-individual"] {
        for id in ["qwen3.8-flash", "qwen3.8-max"] {
            let map = serde_json::to_value(builtin(provider, id).thinking_level_map).unwrap();
            assert_matches(&map, &json!({"minimal": null, "low": "low", "medium": "medium", "high": null, "xhigh": "xhigh", "max": null}), &format!("{provider}/{id}"));
        }
    }
}

#[tokio::test]
async fn qwen_token_plan_thinking_payloads() {
    let high = StreamOptions { reasoning: Some(ThinkingLevel::High), ..Default::default() };
    for (provider, id) in qwen_cases(&QWEN_THINKING_MODELS, &QWEN_INDIVIDUAL_TEXT_MODELS) {
        let payload = simple_payload(&builtin(provider, id), &user_ctx("Hi"), high.clone()).await;
        assert_eq!(payload["enable_thinking"], true, "{provider}/{id}");
        assert!(payload.get("thinking").is_none(), "{provider}/{id}");
    }
    for (provider, id) in qwen_cases(
        &["deepseek-v4-flash", "deepseek-v4-pro", "glm-5", "glm-5.1", "glm-5.2"],
        &["deepseek-v4-flash-0731", "deepseek-v4-pro", "deepseek-v4-pro-0813", "glm-5.2"],
    ) {
        let payload = simple_payload(&builtin(provider, id), &user_ctx("Hi"), high.clone()).await;
        assert_eq!(payload["reasoning_effort"], "high", "{provider}/{id}");
    }
    let xhigh = StreamOptions { reasoning: Some(ThinkingLevel::Xhigh), ..Default::default() };
    for provider in ["qwen-token-plan", "qwen-token-plan-cn", "qwen-token-plan-individual"] {
        for id in ["qwen3.8-flash", "qwen3.8-max"] {
            let payload = simple_payload(&builtin(provider, id), &user_ctx("Hi"), xhigh.clone()).await;
            assert_eq!(payload["enable_thinking"], true, "{provider}/{id}");
            assert_eq!(payload["reasoning_effort"], "xhigh", "{provider}/{id}");
            assert!(payload.get("thinking").is_none(), "{provider}/{id}");
        }
    }
}

// ---------------------------------------------------------------------------
// Port of openrouter-cache-control-models.test.ts and model-catalog-types.test.ts
// ---------------------------------------------------------------------------

#[test]
fn openrouter_anthropic_latest_aliases_keep_cache_control() {
    for id in ["~anthropic/claude-fable-latest", "~anthropic/claude-haiku-latest", "~anthropic/claude-opus-latest", "~anthropic/claude-sonnet-latest"] {
        let model = builtin("openrouter", id);
        assert_eq!(model.api, "openai-completions", "{id}");
        assert_eq!(serde_json::to_value(model.compat()).unwrap()["cacheControlFormat"], "anthropic", "{id}");
    }
}

#[test]
fn grouped_catalog_api_routing() {
    for id in ["grok-4.5", "grok-4.6", "grok-4.7", "grok-4.3"] {
        let model = builtin("xai", id);
        assert_eq!((model.api.as_str(), model.id.as_str(), model.provider.as_str()), ("openai-responses", id, "xai"));
    }
    assert_eq!(builtin("github-copilot", "grok-4.5").api, "openai-responses");
    // Regression test for https://github.com/earendil-works/pi/issues/9209
    let gpt: Vec<Model> = get_builtin_models("github-copilot").into_iter().filter(|m| m.id.starts_with("gpt-")).collect();
    assert!(!gpt.is_empty());
    assert!(gpt.iter().all(|m| m.api == "openai-responses"));
    assert_eq!(builtin("github-copilot", "gpt-6-astra").api, "openai-responses");
    for id in ["gpt-6-sol", "gpt-6-luna"] {
        assert_matches(
            &serde_json::to_value(builtin("github-copilot", id)).unwrap(),
            &json!({"api": "openai-responses", "contextWindow": 1000000, "maxTokens": 128000, "thinkingLevelMap": {"off": "none", "max": "max"}}),
            id,
        );
    }
}

// ---------------------------------------------------------------------------
// Port of openrouter-reasoning-options.test.ts (payloads for generated thinking maps)
// ---------------------------------------------------------------------------

fn ox_alpha(map: Option<Value>) -> Model {
    let mut value = json!({"id": "stealth/ox-alpha", "name": "Ox Alpha", "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://example.invalid/v1", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 4096,
        "compat": {"thinkingFormat": "openrouter"}});
    if let Some(map) = map {
        value["thinkingLevelMap"] = map;
    }
    model(value)
}

#[tokio::test]
async fn openrouter_mandatory_reasoning_payloads() {
    // The generator's map for `{ mandatory: true, supported_efforts: ["max", "high", "low"] }`.
    let mandatory = ox_alpha(Some(json!({"off": null, "minimal": null, "low": "low", "medium": null, "high": "high", "xhigh": null, "max": "max"})));
    let payload = simple_payload(&mandatory, &user_ctx("Hello"), StreamOptions::default()).await;
    assert!(payload.get("reasoning").is_none(), "{payload}");
    let payload = simple_payload(&mandatory, &user_ctx("Hello"), StreamOptions { reasoning: Some(ThinkingLevel::Low), ..Default::default() }).await;
    assert_eq!(payload["reasoning"]["effort"], "low");
    let payload = simple_payload(&ox_alpha(None), &user_ctx("Hello"), StreamOptions::default()).await;
    assert_eq!(payload["reasoning"]["effort"], "none");
}

// ---------------------------------------------------------------------------
// Port of fireworks-model-generation.test.ts ("automatically sends native effort for newly cataloged Messages models")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fireworks_messages_models_with_catalog_efforts_send_native_effort() {
    // The generator's output for `[{ type: "toggle" }, { type: "effort", values: ["low", "max"] }]`.
    let model = model(json!({"id": "accounts/fireworks/models/new-reasoner", "name": "new-reasoner", "api": "anthropic-messages", "provider": "fireworks",
        "baseUrl": "https://api.fireworks.ai/inference", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 16384,
        "thinkingLevelMap": {"off": "none", "minimal": null, "low": "low", "medium": null, "high": null, "xhigh": null, "max": "max"},
        "compat": {"forceAdaptiveThinking": true, "allowEmptySignature": true}}));
    assert_eq!(levels(&model), ["off", "low", "max"]);
    let payload = simple_payload(&model, &user_ctx("test"), StreamOptions { reasoning: Some(ThinkingLevel::Max), api_key: Some("test-fireworks-key".into()), ..Default::default() }).await;
    assert_eq!(payload["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(payload["output_config"], json!({"effort": "max"}));
}

// ---------------------------------------------------------------------------
// Port of model-cost-tiers.test.ts (OpenRouter pricing overrides)
// ---------------------------------------------------------------------------

fn openrouter_cost(pricing: Value) -> Value {
    let pricing: OpenRouterPricing = serde_json::from_value(pricing).unwrap();
    serde_json::to_value(open_router_cost(Some(&pricing))).unwrap()
}

fn assert_cost(actual: &Value, expected: &Value) {
    let close = |a: &Value, b: &Value| (a.as_f64().unwrap() - b.as_f64().unwrap()).abs() < 1e-9;
    for key in ["input", "output", "cacheRead", "cacheWrite"] {
        assert!(close(&actual[key], &expected[key]), "{key}: {actual} vs {expected}");
    }
    let (a, e) = (actual.get("tiers").cloned().unwrap_or(Value::Null), expected.get("tiers").cloned().unwrap_or(Value::Null));
    match (a.as_array(), e.as_array()) {
        (Some(a), Some(e)) => {
            assert_eq!(a.len(), e.len(), "{actual}");
            for (a, e) in a.iter().zip(e) {
                assert_eq!(a["inputTokensAbove"], e["inputTokensAbove"]);
                for key in ["input", "output", "cacheRead", "cacheWrite"] {
                    assert!(close(&a[key], &e[key]), "tier {key}: {a} vs {e}");
                }
            }
        }
        (None, None) => {}
        _ => panic!("tiers differ: {actual} vs {expected}"),
    }
}

#[test]
fn openrouter_pricing_overrides_become_tiers() {
    assert_cost(
        &openrouter_cost(json!({"prompt": "0.0000001", "completion": "0.0000005", "input_cache_read": "0.00000001", "input_cache_write": "0.000000125",
            "overrides": [{"min_prompt_tokens": 100000, "prompt": "0.0000005", "completion": "0.0000025", "input_cache_read": "0.00000005", "input_cache_write": "0.000000625"}]})),
        &json!({"input": 0.1, "output": 0.5, "cacheRead": 0.01, "cacheWrite": 0.125,
            "tiers": [{"inputTokensAbove": 100000, "input": 0.5, "output": 2.5, "cacheRead": 0.05, "cacheWrite": 0.625}]}),
    );
    let cost = openrouter_cost(json!({"prompt": "0.000002", "completion": "0.000012", "input_cache_read": "0.0000002", "input_cache_write": "0.000000375",
        "overrides": [{"min_prompt_tokens": 200000, "prompt": "0.000004", "completion": "0.000018"}]}));
    assert_cost(&cost, &json!({"input": 2, "output": 12, "cacheRead": 0.2, "cacheWrite": 0.375,
        "tiers": [{"inputTokensAbove": 200000, "input": 4, "output": 18, "cacheRead": 0.2, "cacheWrite": 0.375}]}));
    assert_cost(
        &openrouter_cost(json!({"prompt": "0.000000132", "completion": "0.000000528", "overrides": [
            {"utc_start": 0, "utc_end": 1600, "prompt": "0.000000132", "completion": "0.000000528"},
            {"utc_days": ["saturday"], "min_prompt_tokens": 1000, "prompt": "0.0000001"}
        ]})),
        &json!({"input": 0.132, "output": 0.528, "cacheRead": 0, "cacheWrite": 0}),
    );
}

// ---------------------------------------------------------------------------
// Port of lax-message-content.test.ts
// ---------------------------------------------------------------------------

#[test]
fn transform_messages_normalizes_null_and_missing_content() {
    let messages: Vec<Message> = serde_json::from_value(json!([
        {"role": "user", "content": null, "timestamp": 1},
        {"role": "assistant", "content": null, "api": "openai-completions", "provider": "openai", "model": "test-model", "usage": empty_usage(), "stopReason": "stop", "timestamp": 1},
        {"role": "toolResult", "toolCallId": "call_1", "toolName": "web_search", "isError": false, "timestamp": 1}
    ]))
    .unwrap();
    let model = model(json!({"id": "test-model", "name": "Test Model", "api": "openai-completions", "provider": "openai", "baseUrl": "https://example.invalid/v1",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 16000}));
    let result = pi_ai::api::transform_messages::transform_messages(&messages, &model, None);
    assert_eq!(result.len(), 3);
    for message in result {
        assert_eq!(serde_json::to_value(&message).unwrap()["content"], json!([]), "{message:?}");
    }
}

// ---------------------------------------------------------------------------
// Port of env-api-keys.test.ts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn environment_api_keys() {
    use pi_ai::env_api_keys::get_env_api_key;
    let _lock = ENV_LOCK.lock().await;
    let keys = |provider: &str| find_env_keys(provider, None);
    {
        let _env = EnvGuard::set(&[("COPILOT_GITHUB_TOKEN", None), ("GH_TOKEN", Some("gh-token")), ("GITHUB_TOKEN", Some("github-token"))]);
        assert_eq!(keys("github-copilot"), None, "generic GitHub tokens are not Copilot credentials");
        assert_eq!(get_env_api_key("github-copilot", None), None);
        let _copilot = EnvGuard::set(&[("COPILOT_GITHUB_TOKEN", Some("copilot-token"))]);
        assert_eq!(keys("github-copilot"), Some(vec!["COPILOT_GITHUB_TOKEN".to_string()]));
        assert_eq!(get_env_api_key("github-copilot", None).as_deref(), Some("copilot-token"));
    }
    {
        let _env = EnvGuard::set(&[("ZAI_CODING_CN_API_KEY", Some("zai-coding-cn-token"))]);
        assert_eq!(keys("zai-coding-cn"), Some(vec!["ZAI_CODING_CN_API_KEY".to_string()]));
        assert_eq!(get_env_api_key("zai-coding-cn", None).as_deref(), Some("zai-coding-cn-token"));
    }
    let anthropic = |auth: Option<&str>, oauth: Option<&str>, api: Option<&str>| {
        EnvGuard::set(&[("ANTHROPIC_AUTH_TOKEN", auth), ("ANTHROPIC_OAUTH_TOKEN", oauth), ("ANTHROPIC_API_KEY", api)])
    };
    {
        let _env = anthropic(Some("auth-token"), Some("oauth-token"), Some("api-key"));
        assert_eq!(
            keys("anthropic"),
            Some(["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"].map(String::from).to_vec())
        );
        assert_eq!(get_env_api_key("anthropic", None).as_deref(), Some("oauth-token"));
    }
    {
        let _env = anthropic(Some("auth-token"), None, None);
        assert_eq!(keys("anthropic"), Some(vec!["ANTHROPIC_AUTH_TOKEN".to_string()]));
        assert_eq!(get_env_api_key("anthropic", None), None);
    }
    {
        let _env = anthropic(None, Some("oauth-token"), None);
        assert_eq!(keys("anthropic"), Some(vec!["ANTHROPIC_OAUTH_TOKEN".to_string()]));
        assert_eq!(get_env_api_key("anthropic", None).as_deref(), Some("oauth-token"));
    }
    {
        let _env = anthropic(None, None, Some("api-key"));
        assert_eq!(get_env_api_key("anthropic", None).as_deref(), Some("api-key"));
    }
}
