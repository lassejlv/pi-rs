//! Generated model catalog (port of `models.generated.ts` and `model-catalog.ts`).
//!
//! The per-provider JSON data comes from the published `@earendil-works/pi-ai` package
//! (`dist/providers/data/*.json`) and is embedded compressed at build time; it is parsed once on first
//! use.

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde_json::Value;

use crate::types::{AnyModel, ClassifierModel, ImageModel, Model};

/// Provider data files in catalog order.
const DATA: &[(&str, pi_embed::Embedded)] = &[
    ("amazon-bedrock", pi_embed::include_str!("../data/amazon-bedrock.json")),
    ("ant-ling", pi_embed::include_str!("../data/ant-ling.json")),
    ("anthropic", pi_embed::include_str!("../data/anthropic.json")),
    ("azure", pi_embed::include_str!("../data/azure.json")),
    ("baseten", pi_embed::include_str!("../data/baseten.json")),
    ("cerebras", pi_embed::include_str!("../data/cerebras.json")),
    ("cloudflare-ai-gateway", pi_embed::include_str!("../data/cloudflare-ai-gateway.json")),
    ("cloudflare-workers-ai", pi_embed::include_str!("../data/cloudflare-workers-ai.json")),
    ("deepseek", pi_embed::include_str!("../data/deepseek.json")),
    ("fireworks", pi_embed::include_str!("../data/fireworks.json")),
    ("github-copilot", pi_embed::include_str!("../data/github-copilot.json")),
    ("google", pi_embed::include_str!("../data/google.json")),
    ("google-vertex", pi_embed::include_str!("../data/google-vertex.json")),
    ("groq", pi_embed::include_str!("../data/groq.json")),
    ("huggingface", pi_embed::include_str!("../data/huggingface.json")),
    ("kimi-coding", pi_embed::include_str!("../data/kimi-coding.json")),
    ("meta", pi_embed::include_str!("../data/meta.json")),
    ("minimax", pi_embed::include_str!("../data/minimax.json")),
    ("minimax-cn", pi_embed::include_str!("../data/minimax-cn.json")),
    ("mistral", pi_embed::include_str!("../data/mistral.json")),
    ("moonshotai", pi_embed::include_str!("../data/moonshotai.json")),
    ("moonshotai-cn", pi_embed::include_str!("../data/moonshotai-cn.json")),
    ("nvidia", pi_embed::include_str!("../data/nvidia.json")),
    ("openai", pi_embed::include_str!("../data/openai.json")),
    ("openai-codex", pi_embed::include_str!("../data/openai-codex.json")),
    ("opencode", pi_embed::include_str!("../data/opencode.json")),
    ("opencode-go", pi_embed::include_str!("../data/opencode-go.json")),
    ("openrouter", pi_embed::include_str!("../data/openrouter.json")),
    ("qwen-token-plan", pi_embed::include_str!("../data/qwen-token-plan.json")),
    ("qwen-token-plan-cn", pi_embed::include_str!("../data/qwen-token-plan-cn.json")),
    ("qwen-token-plan-individual", pi_embed::include_str!("../data/qwen-token-plan-individual.json")),
    ("radius", pi_embed::include_str!("../data/radius.json")),
    ("together", pi_embed::include_str!("../data/together.json")),
    ("typesafe", pi_embed::include_str!("../data/typesafe.json")),
    ("vercel-ai-gateway", pi_embed::include_str!("../data/vercel-ai-gateway.json")),
    ("xai", pi_embed::include_str!("../data/xai.json")),
    ("xiaomi", pi_embed::include_str!("../data/xiaomi.json")),
    ("xiaomi-token-plan-ams", pi_embed::include_str!("../data/xiaomi-token-plan-ams.json")),
    ("xiaomi-token-plan-cn", pi_embed::include_str!("../data/xiaomi-token-plan-cn.json")),
    ("xiaomi-token-plan-sgp", pi_embed::include_str!("../data/xiaomi-token-plan-sgp.json")),
    ("zai", pi_embed::include_str!("../data/zai.json")),
    ("zai-coding-cn", pi_embed::include_str!("../data/zai-coding-cn.json")),
];

const MANIFEST: pi_embed::Embedded = pi_embed::include_str!("../data/.manifest.json");

/// A flattened catalog: provider id -> model id -> model.
pub struct Catalog {
    pub chat: IndexMap<String, IndexMap<String, Model>>,
    pub image: IndexMap<String, IndexMap<String, ImageModel>>,
    pub classifier: IndexMap<String, IndexMap<String, ClassifierModel>>,
}

fn parse_catalog() -> Catalog {
    let mut catalog = Catalog { chat: IndexMap::new(), image: IndexMap::new(), classifier: IndexMap::new() };
    for (provider, json) in DATA {
        let groups: IndexMap<String, IndexMap<String, Value>> =
            serde_json::from_str(&json.decompress()).unwrap_or_else(|e| panic!("invalid model data for {provider}: {e}"));
        let chat = catalog.chat.entry(provider.to_string()).or_default();
        let mut images = IndexMap::new();
        let mut classifiers = IndexMap::new();
        for models in groups.into_values() {
            for value in models.into_values() {
                let model = AnyModel::from_value(value)
                    .unwrap_or_else(|e| panic!("invalid model entry for {provider}: {e}"));
                match model {
                    AnyModel::Chat(m) => {
                        chat.insert(m.id.clone(), m);
                    }
                    AnyModel::Image(m) => {
                        images.insert(m.id.clone(), m);
                    }
                    AnyModel::Classifier(m) => {
                        classifiers.insert(m.id.clone(), m);
                    }
                }
            }
        }
        catalog.image.insert(provider.to_string(), images);
        catalog.classifier.insert(provider.to_string(), classifiers);
    }
    catalog
}

/// The built-in catalog, parsed on first use.
pub static CATALOG: Lazy<Catalog> = Lazy::new(parse_catalog);

/// Generation timestamp of the built-in catalog (ISO string from the manifest).
pub fn manifest_generated_at() -> Option<String> {
    let manifest: Value = serde_json::from_str(&MANIFEST.decompress()).ok()?;
    manifest.get("generatedAt").and_then(Value::as_str).map(str::to_string)
}

/// Provider ids in catalog order.
pub fn catalog_providers() -> Vec<&'static str> {
    DATA.iter().map(|(p, _)| *p).collect()
}
