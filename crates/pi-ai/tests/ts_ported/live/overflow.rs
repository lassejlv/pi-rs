//! Live port of context-overflow.test.ts: every provider's overflow error must satisfy
//! `is_context_overflow`.

use pi_ai::compat::complete;
use pi_ai::providers::all::get_builtin_models;
use pi_ai::utils::overflow::is_context_overflow;
use pi_ai::*;
use regex::Regex;
use serde_json::json;

use super::*;
use crate::support::ctx;

const LOREM_IPSUM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum. ";

fn overflow_content(context_window: u64) -> String {
    let target_chars = (context_window + 10_000) as f64 * 4.0 * 1.5;
    LOREM_IPSUM.repeat((target_chars / LOREM_IPSUM.len() as f64).ceil() as usize)
}

async fn overflow(model: &Model, options: StreamOptions) -> AssistantMessage {
    let context = ctx(json!({"systemPrompt": "You are a helpful assistant.",
        "messages": [{"role": "user", "content": overflow_content(model.context_window), "timestamp": now()}]}));
    let response = complete(model, &context, options).await;
    println!(
        "\n{} / {}:\n  contextWindow: {}\n  stopReason: {:?}\n  errorMessage: {:?}\n  usage: {:?}",
        model.provider, model.id, model.context_window, response.stop_reason, response.error_message, response.usage
    );
    response
}

/// The error-message pattern each TS case checks (if any).
fn error_pattern(model: &Model) -> Option<&'static str> {
    Some(match (model.provider.as_str(), model.id.as_str()) {
        ("anthropic", _) => "(?i)prompt is too long",
        ("github-copilot", id) if id.starts_with("gemini-") => r"(?i)exceeds the limit of \d+",
        ("github-copilot", _) => r"(?i)exceeds the limit of \d+|input is too long",
        ("openai", _) if model.api == "openai-completions" => "(?i)maximum context length",
        ("openai", _) => "(?i)exceeds the context window",
        ("azure", _) => "(?i)context|maximum",
        ("google", _) => "(?i)input token count.*exceeds the maximum",
        ("xai", _) => r"(?i)maximum prompt length is \d+",
        ("groq", _) => "(?i)reduce the length of the messages",
        ("cerebras", _) => r"(?i)4(00|13|29).*\(no body\)",
        ("mistral", _) => r"(?i)too large for model with \d+ maximum context length",
        ("qwen-token-plan" | "qwen-token-plan-individual" | "qwen-token-plan-cn", _) => "(?i)input length",
        ("openrouter", _) => r"(?i)maximum context length is \d+ tokens",
        _ => return None,
    })
}

fn detects_overflow(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let response = overflow(model, options).await;
        match model.provider.as_str() {
            // z.ai either reports the overflow or silently accepts it.
            "zai" => match response.stop_reason {
                StopReason::Error => {
                    if response.error_message.as_deref().is_some_and(|m| m.to_lowercase().contains("model_context_window_exceeded")) {
                        assert!(is_context_overflow(&response, Some(model.context_window)));
                    } else {
                        println!("  z.ai returned a non-overflow error");
                    }
                }
                StopReason::Stop => {
                    if response.usage.input + response.usage.cache_read > 0 && response.usage.input > model.context_window {
                        assert!(is_context_overflow(&response, Some(model.context_window)));
                    } else {
                        println!("  z.ai accepted the request without reporting overflow");
                    }
                }
                _ => {}
            },
            // Xiaomi stops with `length` and no output once the context is full.
            "xiaomi" | "xiaomi-token-plan-cn" | "xiaomi-token-plan-ams" | "xiaomi-token-plan-sgp" => {
                assert_eq!(response.stop_reason, StopReason::Length);
                assert_eq!(response.usage.output, 0);
                assert!(is_context_overflow(&response, Some(model.context_window)));
            }
            // Ollama silently truncates.
            "ollama" => {
                if response.stop_reason == StopReason::Error {
                    assert!(is_context_overflow(&response, Some(model.context_window)));
                }
            }
            _ => {
                assert_eq!(response.stop_reason, StopReason::Error, "{:?}", response.error_message);
                if let Some(pattern) = error_pattern(model) {
                    let message = response.error_message.as_deref().unwrap_or_default();
                    assert!(Regex::new(pattern).unwrap().is_match(message), "{message:?} !~ {pattern}");
                }
                assert!(is_context_overflow(&response, Some(model.context_window)), "{:?}", response.error_message);
            }
        }
    })
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[allow(clippy::too_many_arguments)]
fn local(label: &str, gate: Gate, provider: &'static str, id: &str, base_url: &str, name: &str, reasoning: bool, window: u64, max: u64) -> Target {
    target(label, gate, provider, "local")
        .custom(json!({"id": id, "api": "openai-completions", "provider": provider, "baseUrl": base_url, "reasoning": reasoning,
            "input": ["text"], "contextWindow": window, "maxTokens": max, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "name": name}))
        .api_key(provider)
}

pub fn targets() -> Vec<Target> {
    // `getModels("github-copilot").find((m) => m.id.startsWith("gemini-"))`.
    let copilot_gemini = get_builtin_models("github-copilot").into_iter().find(|m| m.id.starts_with("gemini-")).map(|m| m.id);
    // The first preferred Cerebras model in catalog order, else the first one.
    let cerebras = get_builtin_models("cerebras");
    let cerebras_id = cerebras
        .iter()
        .find(|m| ["gpt-oss-120b", "zai-glm-4.7", "llama3.1-8b"].contains(&m.id.as_str()))
        .or(cerebras.first())
        .map(|m| m.id.clone())
        .unwrap_or_default();
    let mut targets = vec![
        target("Anthropic (API Key)", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-haiku-4-5"),
        target("Anthropic (OAuth)", Gate::Env(&["ANTHROPIC_OAUTH_TOKEN"]), "anthropic", "claude-sonnet-4-6")
            .api_key(&env("ANTHROPIC_OAUTH_TOKEN").unwrap_or_default()),
    ];
    match copilot_gemini {
        Some(id) => targets.push(target("GitHub Copilot Google model", Gate::OAuth("github-copilot"), "github-copilot", leak(id))),
        None => println!("No Google models available through GitHub Copilot"),
    }
    targets.extend([
        target("GitHub Copilot claude-sonnet-4.6", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
        // `{ ...getModel(...) }` with `api` reassigned: compat is kept.
        target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").with_api("openai-completions"),
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o"),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini").api_key(&env("AZURE_OPENAI_API_KEY").unwrap_or_default()),
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-5.6-sol"),
        target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"),
        target("xAI", Gate::Env(&["XAI_API_KEY"]), "xai", "grok-4.3"),
        target("Groq", Gate::Env(&["GROQ_API_KEY"]), "groq", "llama-3.3-70b-versatile"),
        target("Cerebras", Gate::Env(&["CEREBRAS_API_KEY"]), "cerebras", leak(cerebras_id)),
        target("Hugging Face", Gate::Env(&["HF_TOKEN"]), "huggingface", "moonshotai/Kimi-K2.5"),
        target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3"),
        target("z.ai", Gate::Env(&["ZAI_API_KEY"]), "zai", "glm-5.2"),
        target("Mistral", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "devstral-medium-latest"),
        target("MiniMax", Gate::Env(&["MINIMAX_API_KEY"]), "minimax", "MiniMax-M2.7"),
        target("Xiaomi MiMo (API billing)", Gate::Env(&["XIAOMI_API_KEY"]), "xiaomi", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (CN)", Gate::Env(&["XIAOMI_TOKEN_PLAN_CN_API_KEY"]), "xiaomi-token-plan-cn", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (AMS)", Gate::Env(&["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]), "xiaomi-token-plan-ams", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (SGP)", Gate::Env(&["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]), "xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
        target("Qwen Token Plan", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan", "qwen3.7-max"),
        target("Qwen Token Plan Individual", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan-individual", "qwen3.8-max"),
        target("Qwen Token Plan (CN)", Gate::Env(&["QWEN_TOKEN_PLAN_CN_API_KEY"]), "qwen-token-plan-cn", "qwen3.7-max"),
        target("Kimi For Coding", Gate::Env(&["KIMI_API_KEY"]), "kimi-coding", "kimi-for-coding"),
        target("Vercel AI Gateway", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "google/gemini-2.5-flash"),
        target("OpenRouter anthropic/claude-sonnet-4.5", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "anthropic/claude-sonnet-4.5"),
        target("OpenRouter deepseek/deepseek-v3.2", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "deepseek/deepseek-v3.2"),
        target("OpenRouter mistralai/mistral-large", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "mistralai/mistral-large"),
        target("OpenRouter google/gemini-2.5-flash", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "google/gemini-2.5-flash"),
        target("OpenRouter meta-llama/llama-4-scout", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "meta-llama/llama-4-scout"),
        local("Ollama (local)", Gate::Ollama, "ollama", "gpt-oss:20b", "http://localhost:11434/v1", "Ollama GPT-OSS 20B", true, 128000, 16000),
        local("LM Studio (local)", Gate::LmStudio, "lm-studio", "local-model", "http://localhost:1234/v1", "LM Studio Local Model", false, 8192, 2048),
        local("llama.cpp (local)", Gate::LlamaCpp, "llama.cpp", "local-model", "http://localhost:8081/v1", "llama.cpp Local Model", false, 4096, 2048),
    ]);
    targets
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn context_overflow_e2e() {
    run_matrix("context-overflow.test.ts", targets(), &[("should detect overflow via isContextOverflow", detects_overflow)], 0).await;
}
