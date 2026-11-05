//! Live ports of the E2E blocks of tokens.test.ts ("Token Statistics on Abort") and
//! images.test.ts (OpenRouter image generation).

use pi_ai::compat::stream;
use pi_ai::providers::all::{get_builtin_image_model, get_builtin_models};
use pi_ai::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::matrix::red_circle_base64;
use super::*;
use crate::support::ctx;

fn tokens_on_abort(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "Write a long poem with 20 stanzas about the beauty of nature.", "timestamp": now()}]}));
        let signal = CancellationToken::new();
        let mut events = stream(model, &context, StreamOptions { signal: Some(signal.clone()), ..options });
        let mut text = String::new();
        while let Some(event) = events.next_event().await {
            if signal.is_cancelled() {
                continue;
            }
            if let AssistantMessageEvent::TextDelta { delta, .. } | AssistantMessageEvent::ThinkingDelta { delta, .. } = &event {
                text.push_str(delta);
                if text.len() >= 1000 {
                    signal.cancel();
                }
            }
        }
        let message = events.result().await;
        assert_eq!(message.stop_reason, StopReason::Aborted, "{:?}", message.error_message);
        let usage = &message.usage;
        // OpenAI-style APIs, zai, Bedrock and the Vercel gateway send usage only in the final chunk.
        let final_chunk_usage = matches!(
            model.api.as_str(),
            "openai-completions" | "mistral-conversations" | "openai-responses" | "azure-openai-responses" | "openai-codex-responses"
        ) || matches!(model.provider.as_str(), "zai" | "amazon-bedrock" | "vercel-ai-gateway");
        if final_chunk_usage || model.provider == "minimax" {
            assert_eq!((usage.input, usage.output), (0, 0));
        } else if model.provider == "kimi-coding" {
            assert!(usage.input > 0);
            assert_eq!(usage.output, 0);
        } else {
            assert!(usage.input > 0 && usage.output > 0, "{usage:?}");
            if model.cost.input > 0.0 {
                assert!(usage.cost.input > 0.0 && usage.cost.total > 0.0, "{usage:?}");
            }
        }
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn token_statistics_on_abort_e2e() {
    let cerebras = get_builtin_models("cerebras");
    let cerebras_id = cerebras
        .iter()
        .find(|m| ["gpt-oss-120b", "zai-glm-4.7", "llama3.1-8b"].contains(&m.id.as_str()))
        .or(cerebras.first())
        .map(|m| m.id.clone())
        .expect("No Cerebras models available");
    let targets = vec![
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
        target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5.4-mini").extra("reasoningEffort", json!("low")),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
        target("Anthropic", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-sonnet-4-6"),
        target("xAI", Gate::Env(&["XAI_API_KEY"]), "xai", "grok-4.3"),
        target("Groq", Gate::Env(&["GROQ_API_KEY"]), "groq", "openai/gpt-oss-20b"),
        target("Cerebras", Gate::Env(&["CEREBRAS_API_KEY"]), "cerebras", Box::leak(cerebras_id.into_boxed_str())),
        target("Cloudflare Workers AI", Gate::CloudflareWorkers, "cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        target("Cloudflare AI Gateway", Gate::CloudflareGateway, "cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
        target("Hugging Face", Gate::Env(&["HF_TOKEN"]), "huggingface", "moonshotai/Kimi-K2.5"),
        target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3"),
        target("Baseten", Gate::Env(&["BASETEN_API_KEY"]), "baseten", "zai-org/GLM-5.2").extra("reasoningEffort", json!("high")),
        target("zAI", Gate::Env(&["ZAI_API_KEY"]), "zai", "glm-5.2"),
        target("Mistral", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "devstral-medium-latest"),
        target("MiniMax", Gate::Env(&["MINIMAX_API_KEY"]), "minimax", "MiniMax-M2.7"),
        target("Kimi For Coding", Gate::Env(&["KIMI_API_KEY"]), "kimi-coding", "kimi-for-coding"),
        target("Meta", Gate::Env(&["META_API_KEY"]), "meta", "muse-spark-1.3"),
        target("Vercel AI Gateway", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "google/gemini-2.5-flash"),
        target("Xiaomi MiMo (API billing)", Gate::Env(&["XIAOMI_API_KEY"]), "xiaomi", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (CN)", Gate::Env(&["XIAOMI_TOKEN_PLAN_CN_API_KEY"]), "xiaomi-token-plan-cn", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (AMS)", Gate::Env(&["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]), "xiaomi-token-plan-ams", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (SGP)", Gate::Env(&["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]), "xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
        target("Qwen Token Plan", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan", "qwen3.7-max"),
        target("Qwen Token Plan Individual", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan-individual", "qwen3.8-max"),
        target("Qwen Token Plan (CN)", Gate::Env(&["QWEN_TOKEN_PLAN_CN_API_KEY"]), "qwen-token-plan-cn", "qwen3.7-max"),
        target("Anthropic OAuth", Gate::OAuth("anthropic"), "anthropic", "claude-sonnet-4-6"),
        target("GitHub Copilot claude-haiku-4.5", Gate::OAuth("github-copilot"), "github-copilot", "claude-haiku-4.5"),
        target("GitHub Copilot claude-sonnet-4.6", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
        target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"),
    ];
    run_matrix("tokens.test.ts", targets, &[("should include token stats when aborted mid-stream", tokens_on_abort)], 3).await;
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn openrouter_images_e2e() {
    const FILE: &str = "images.test.ts";
    run_case(FILE, "OpenRouter: should generate a basic image", &[Gate::Env(&["OPENROUTER_API_KEY"])], 3, |_| {
        Box::pin(async {
            let model = get_builtin_image_model("openrouter", "google/gemini-2.5-flash-image").expect("image model");
            let context = ImagesContext { input: vec![UserContentBlock::text("Generate a simple red circle on a plain white background. No text.")] };
            let response = pi_ai::images::generate_images(&model, &context, StreamOptions::default()).await.unwrap();
            assert_eq!(serde_json::to_value(response.stop_reason).unwrap(), "stop", "Error: {:?}", response.error_message);
            assert!(response.error_message.as_deref().unwrap_or_default().is_empty());
            assert!(response.output.iter().any(|item| matches!(item, UserContentBlock::Image(_))));
            assert!(response.timestamp > 0);
        })
    })
    .await;
    run_case(FILE, "OpenRouter: should handle image input", &[Gate::Env(&["OPENROUTER_API_KEY"])], 3, |_| {
        Box::pin(async {
            let model = get_builtin_image_model("openrouter", "google/gemini-2.5-flash-image").expect("image model");
            if !serde_json::to_value(&model.input).unwrap().as_array().unwrap().contains(&json!("image")) {
                println!("Skipping image input test - model {} doesn't support image input", model.id);
                return;
            }
            let context = ImagesContext {
                input: vec![
                    UserContentBlock::text("Create a variation of this image with a blue background."),
                    UserContentBlock::Image(ImageContent { data: red_circle_base64(), mime_type: "image/png".into() }),
                ],
            };
            let response = pi_ai::images::generate_images(&model, &context, StreamOptions::default()).await.unwrap();
            assert_eq!(serde_json::to_value(response.stop_reason).unwrap(), "stop", "Error: {:?}", response.error_message);
            assert!(response.output.iter().any(|item| matches!(item, UserContentBlock::Image(_))));
        })
    })
    .await;
}
