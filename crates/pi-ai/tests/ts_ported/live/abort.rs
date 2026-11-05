//! Live port of abort.test.ts (offline abort handling is covered by the per-provider mock tests).

use pi_ai::compat::{complete, stream};
use pi_ai::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::support::ctx;

fn abort_mid_stream(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let mut context = ctx(json!({"systemPrompt": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "What is 15 + 27? Think step by step. Then list 50 first names.", "timestamp": now()}]}));
        let signal = CancellationToken::new();
        let mut events = stream(model, &context, StreamOptions { signal: Some(signal.clone()), ..options.clone() });
        let mut text = String::new();
        while let Some(event) = events.next_event().await {
            if signal.is_cancelled() {
                // The TS loop returns as soon as it sees an event after aborting.
                return;
            }
            if let AssistantMessageEvent::TextDelta { delta, .. } | AssistantMessageEvent::ThinkingDelta { delta, .. } = &event {
                text.push_str(delta);
            }
            if text.len() >= 50 {
                signal.cancel();
            }
        }
        let message = events.result().await;
        assert_eq!(message.stop_reason, StopReason::Aborted, "{:?}", message.error_message);
        assert!(!message.content.is_empty());
        context.messages.push(Message::Assistant(message));
        context.messages.push(user_string("Please continue, but only generate 5 names."));
        let follow_up = complete(model, &context, options).await;
        assert_eq!(follow_up.stop_reason, StopReason::Stop, "{:?}", follow_up.error_message);
        assert!(!follow_up.content.is_empty());
    })
}

fn immediate_abort(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let signal = CancellationToken::new();
        signal.cancel();
        let context = ctx(json!({"messages": [{"role": "user", "content": "Hello", "timestamp": now()}]}));
        let response = complete(model, &context, StreamOptions { signal: Some(signal), ..options }).await;
        assert_eq!(response.stop_reason, StopReason::Aborted);
    })
}

fn abort_then_new_message(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let signal = CancellationToken::new();
        signal.cancel();
        let mut context = ctx(json!({"messages": [{"role": "user", "content": "Hello, how are you?", "timestamp": now()}]}));
        let aborted = complete(model, &context, StreamOptions { signal: Some(signal), ..options.clone() }).await;
        assert_eq!(aborted.stop_reason, StopReason::Aborted);
        assert!(aborted.content.is_empty());
        context.messages.push(Message::Assistant(aborted));
        context.messages.push(user_string("What is 2 + 2?"));
        let follow_up = complete(model, &context, options).await;
        assert_eq!(follow_up.stop_reason, StopReason::Stop, "{:?}", follow_up.error_message);
        assert!(!follow_up.content.is_empty());
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn abort_e2e() {
    const FILE: &str = "abort.test.ts";
    let high = |t: Target| t.extra("reasoningEffort", json!("high"));
    let targets = vec![
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash").extra("thinking", json!({"enabled": true})),
        target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5-mini"),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
        target("Anthropic", Gate::Env(&["ANTHROPIC_OAUTH_TOKEN"]), "anthropic", "claude-sonnet-4-6")
            .extra("thinkingEnabled", json!(true))
            .extra("thinkingBudgetTokens", json!(2048)),
        target("Mistral", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "devstral-medium-latest"),
        high(target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3")),
        high(target("Baseten", Gate::Env(&["BASETEN_API_KEY"]), "baseten", "zai-org/GLM-5.2")),
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
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
    ];
    run_matrix(FILE, targets, &[("should abort mid-stream", abort_mid_stream), ("should handle immediate abort", immediate_abort)], 3).await;

    // Amazon Bedrock: reasoning only for the mid-stream case, plus the abort-then-new-message case.
    let bedrock = || target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let Some((model, options)) = resolve(&bedrock()).await else {
        println!("[{FILE}] skip Amazon Bedrock (Bedrock not configured)");
        return;
    };
    let mut failures = Vec::new();
    let medium = StreamOptions { reasoning: Some(serde_json::from_value(json!("medium")).unwrap()), ..options.clone() };
    for (name, scenario, options) in [
        ("should abort mid-stream", abort_mid_stream as Scenario, medium),
        ("should handle immediate abort", immediate_abort as Scenario, options.clone()),
        ("should handle abort then new message", abort_then_new_message as Scenario, options),
    ] {
        let model = model.clone();
        if let Err(error) = attempt(3, move || {
            let model = model.clone();
            let options = options.clone();
            Box::pin(async move { scenario(&model, options).await })
        })
        .await
        {
            failures.push(format!("Amazon Bedrock :: {name}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
