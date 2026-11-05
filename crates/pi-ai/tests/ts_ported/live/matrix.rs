//! Live ports of the provider-matrix E2E files: empty.test.ts, tool-call-without-result.test.ts,
//! unicode-surrogate.test.ts, image-tool-result.test.ts and total-tokens.test.ts.

use base64::Engine;
use pi_ai::compat::complete;
use pi_ai::*;
use serde_json::{Value, json};

use super::*;
use crate::support::ctx;

/// The provider matrix shared by the "every provider" E2E files.
pub struct MatrixOptions {
    /// `{ reasoningEffort: "high" }` for Together AI.
    pub together_high: bool,
    /// `{ reasoningEffort: "high" }` for Baseten.
    pub baseten_high: bool,
    /// The OpenAI Completions block rebuilds gpt-4o-mini with `api: "openai-completions"`.
    pub openai_completions_api: bool,
}

pub fn standard_matrix(options: MatrixOptions) -> Vec<Target> {
    let mut completions = target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini");
    if options.openai_completions_api {
        completions = completions.api_without_compat("openai-completions");
    }
    let mut together = target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3");
    if options.together_high {
        together = together.extra("reasoningEffort", json!("high"));
    }
    let mut baseten = target("Baseten", Gate::Env(&["BASETEN_API_KEY"]), "baseten", "zai-org/GLM-5.2");
    if options.baseten_high {
        baseten = baseten.extra("reasoningEffort", json!("high"));
    }
    vec![
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
        completions,
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5-mini"),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
        target("Anthropic", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-haiku-4-5"),
        target("xAI", Gate::Env(&["XAI_API_KEY"]), "xai", "grok-4.3"),
        target("Groq", Gate::Env(&["GROQ_API_KEY"]), "groq", "openai/gpt-oss-20b"),
        target("Cerebras", Gate::Env(&["CEREBRAS_API_KEY"]), "cerebras", "gpt-oss-120b"),
        target("Cloudflare Workers AI", Gate::CloudflareWorkers, "cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        target("Cloudflare AI Gateway", Gate::CloudflareGateway, "cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
        target("Hugging Face", Gate::Env(&["HF_TOKEN"]), "huggingface", "moonshotai/Kimi-K2.5"),
        together,
        baseten,
        target("zAI", Gate::Env(&["ZAI_API_KEY"]), "zai", "glm-5.2"),
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
        target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"),
        target("Anthropic OAuth", Gate::OAuth("anthropic"), "anthropic", "claude-haiku-4-5"),
        target("GitHub Copilot claude-haiku-4.5", Gate::OAuth("github-copilot"), "github-copilot", "claude-haiku-4.5"),
        target("GitHub Copilot claude-sonnet-4.6", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
    ]
}

fn assistant_value(model: &Model, content: Value, stop_reason: &str, input: u64) -> Value {
    json!({"role": "assistant", "content": content, "api": model.api, "provider": model.provider, "model": model.id,
        "usage": {"input": input, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": input,
                  "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": stop_reason, "timestamp": now()})
}

#[track_caller]
fn expect_graceful(response: &AssistantMessage) {
    if response.stop_reason == StopReason::Error {
        assert!(response.error_message.is_some());
    }
}

// ---------------------------------------------------------------------------
// empty.test.ts
// ---------------------------------------------------------------------------

fn empty_content_array(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"messages": [{"role": "user", "content": [], "timestamp": now()}]}));
        expect_graceful(&complete(model, &context, options).await);
    })
}

fn empty_string(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"messages": [{"role": "user", "content": "", "timestamp": now()}]}));
        expect_graceful(&complete(model, &context, options).await);
    })
}

fn whitespace_only(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"messages": [{"role": "user", "content": "   \n\t  ", "timestamp": now()}]}));
        expect_graceful(&complete(model, &context, options).await);
    })
}

fn empty_assistant_in_conversation(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"messages": [
            {"role": "user", "content": "Hello, how are you?", "timestamp": now()},
            assistant_value(model, json!([]), "stop", 10),
            {"role": "user", "content": "Please respond this time.", "timestamp": now()}
        ]}));
        let response = complete(model, &context, options).await;
        expect_graceful(&response);
        if response.stop_reason != StopReason::Error {
            assert!(!response.content.is_empty());
        }
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn empty_messages_e2e() {
    let targets = standard_matrix(MatrixOptions { together_high: false, baseten_high: true, openai_completions_api: false });
    run_matrix(
        "empty.test.ts",
        targets,
        &[
            ("should handle empty content array", empty_content_array),
            ("should handle empty string content", empty_string),
            ("should handle whitespace-only content", whitespace_only),
            ("should handle empty assistant message in conversation", empty_assistant_in_conversation),
        ],
        3,
    )
    .await;
}

// ---------------------------------------------------------------------------
// tool-call-without-result.test.ts
// ---------------------------------------------------------------------------

fn calculate_tool() -> Value {
    json!({"name": "calculate", "description": "Evaluate mathematical expressions", "parameters": {"type": "object",
        "properties": {"expression": {"type": "string", "description": "The mathematical expression to evaluate"}}, "required": ["expression"]}})
}

fn tool_call_without_result(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let mut context = ctx(json!({
            "systemPrompt": "You are a helpful assistant. Use the calculate tool when asked to perform calculations.",
            "messages": [{"role": "user", "content": "Please calculate 25 * 18 using the calculate tool.", "timestamp": now()}],
            "tools": [calculate_tool()]
        }));
        let first = complete(model, &context, options.clone()).await;
        assert!(has_tool_call(&first), "expected a tool call: {:?}", first.error_message);
        context.messages.push(Message::Assistant(first));
        // A user message without the tool result: the orphaned call must be filtered out.
        context.messages.push(user_string("Never mind, just tell me what is 2+2?"));
        let second = complete(model, &context, options).await;
        expect_not_error(&second);
        assert!(!second.content.is_empty());
        let tool_calls = second.content.iter().filter(|b| matches!(b, AssistantContent::ToolCall(_))).count();
        assert!(tool_calls > 0 || !text_of(&second).is_empty());
        assert!(matches!(second.stop_reason, StopReason::Stop | StopReason::ToolUse), "{:?}", second.stop_reason);
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn tool_call_without_result_e2e() {
    let targets = standard_matrix(MatrixOptions { together_high: true, baseten_high: true, openai_completions_api: true });
    run_matrix(
        "tool-call-without-result.test.ts",
        targets,
        &[("should filter out tool calls without corresponding tool results", tool_call_without_result)],
        3,
    )
    .await;
}

// ---------------------------------------------------------------------------
// unicode-surrogate.test.ts
// ---------------------------------------------------------------------------

fn tool_result_context(model: &Model, id: &str, tool: &str, description: &str, ask: &str, result: &str, follow_up: &str) -> Context {
    ctx(json!({
        "systemPrompt": "You are a helpful assistant.",
        "messages": [
            {"role": "user", "content": ask, "timestamp": now()},
            assistant_value(model, json!([{"type": "toolCall", "id": id, "name": tool, "arguments": {}}]), "toolUse", 0),
            {"role": "toolResult", "toolCallId": id, "toolName": tool, "content": [{"type": "text", "text": result}], "isError": false, "timestamp": now()},
            {"role": "user", "content": follow_up, "timestamp": now()}
        ],
        "tools": [{"name": tool, "description": description, "parameters": {"type": "object", "properties": {}}}]
    }))
}

fn call_id(model: &Model, mistral: &'static str, other: &'static str) -> &'static str {
    if model.provider == "mistral" { mistral } else { other }
}

fn emoji_in_tool_results(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let text = "Test with emoji 🙈 and other characters:\n- Monkey emoji: 🙈\n- Thumbs up: 👍\n- Heart: ❤️\n- Thinking face: 🤔\n- Rocket: 🚀\n- Mixed text: Mario Zechner wann? Wo? Bin grad äußersr eventuninformiert 🙈\n- Japanese: こんにちは\n- Chinese: 你好\n- Mathematical symbols: ∑∫∂√\n- Special quotes: \u{201c}curly\u{201d} \u{2018}quotes\u{2019}";
        let context = tool_result_context(
            model,
            call_id(model, "testtool1", "test_1"),
            "test_tool",
            "A test tool",
            "Use the test tool",
            text,
            "Summarize the tool result briefly.",
        );
        let response = complete(model, &context, options).await;
        expect_not_error(&response);
        assert!(response.error_message.as_deref().unwrap_or_default().is_empty());
        assert!(!response.content.is_empty());
    })
}

fn linkedin_data(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let text = "Post: Hab einen \"Generative KI für Nicht-Techniker\" Workshop gebaut.\nUnanswered Comments: 2\n\n=> {\n  \"comments\": [\n    {\n      \"author\": \"Matthias Neumayer's  graphic link\",\n      \"text\": \"Leider nehmen das viel zu wenige Leute ernst\"\n    },\n    {\n      \"author\": \"Matthias Neumayer's  graphic link\",\n      \"text\": \"Mario Zechner wann? Wo? Bin grad äußersr eventuninformiert 🙈\"\n    }\n  ]\n}";
        let context = tool_result_context(
            model,
            call_id(model, "linkedin1", "linkedin_1"),
            "linkedin_skill",
            "Get LinkedIn comments",
            "Use the linkedin tool to get comments",
            text,
            "How many comments are there?",
        );
        let response = complete(model, &context, options).await;
        expect_not_error(&response);
        assert!(response.error_message.as_deref().unwrap_or_default().is_empty());
        assert!(response.content.iter().any(|b| matches!(b, AssistantContent::Text(_))));
    })
}

fn unpaired_high_surrogate(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        // A Rust `String` cannot hold a lone surrogate: every boundary that decodes one (JSON with
        // `\ud83d`, lossy UTF-16) yields U+FFFD, which is what reaches the provider here.
        let text = "Text with unpaired surrogate: \u{FFFD} <- should be sanitized";
        let context = tool_result_context(
            model,
            call_id(model, "testtool2", "test_2"),
            "test_tool",
            "A test tool",
            "Use the test tool",
            text,
            "What did the tool return?",
        );
        let response = complete(model, &context, options).await;
        expect_not_error(&response);
        assert!(response.error_message.as_deref().unwrap_or_default().is_empty());
        assert!(!response.content.is_empty());
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn unicode_surrogate_e2e() {
    let targets = standard_matrix(MatrixOptions { together_high: true, baseten_high: true, openai_completions_api: false });
    run_matrix(
        "unicode-surrogate.test.ts",
        targets,
        &[
            ("should handle emoji in tool results", emoji_in_tool_results),
            ("should handle real-world LinkedIn comment data with emoji", linkedin_data),
            ("should handle unpaired high surrogate (0xD83D) in tool results", unpaired_high_surrogate),
        ],
        3,
    )
    .await;
}

// ---------------------------------------------------------------------------
// image-tool-result.test.ts
// ---------------------------------------------------------------------------

pub fn red_circle_base64() -> String {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/red-circle.png")).unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn image_tool_round_trip(model: &Model, options: StreamOptions, tool: &str, description: &str, ask: &str, result_text: Option<&str>) -> String {
    let mut context = ctx(json!({
        "systemPrompt": "You are a helpful assistant that uses tools when asked.",
        "messages": [{"role": "user", "content": ask, "timestamp": now()}],
        "tools": [{"name": tool, "description": description, "parameters": {"type": "object", "properties": {}}}]
    }));
    let first = complete(model, &context, options.clone()).await;
    assert_eq!(first.stop_reason, StopReason::ToolUse, "{:?}", first.error_message);
    let call = first.tool_calls().next().expect("tool call").clone();
    assert_eq!(call.name, tool);
    context.messages.push(Message::Assistant(first));
    let mut content = Vec::new();
    if let Some(text) = result_text {
        content.push(json!({"type": "text", "text": text}));
    }
    content.push(json!({"type": "image", "data": red_circle_base64(), "mimeType": "image/png"}));
    context.messages.push(
        serde_json::from_value(json!({"role": "toolResult", "toolCallId": call.id, "toolName": call.name, "content": content,
            "isError": false, "timestamp": now()}))
        .unwrap(),
    );
    let second = complete(model, &context, options).await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{:?}", second.error_message);
    assert!(second.error_message.as_deref().unwrap_or_default().is_empty());
    let text = second
        .content
        .iter()
        .find_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.to_lowercase()),
            _ => None,
        })
        .expect("text content");
    assert!(text.contains("red") && text.contains("circle"), "{text}");
    text
}

fn tool_with_image_result(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        if !model.input.contains(&ModelInputModality::Image) {
            println!("Skipping tool image result test - model {} doesn't support images", model.id);
            return;
        }
        image_tool_round_trip(
            model,
            options,
            "get_circle",
            "Returns a circle image for visualization",
            "Call the get_circle tool to get an image, and describe what you see, shapes, colors, etc.",
            None,
        )
        .await;
    })
}

fn tool_with_text_and_image_result(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        if !model.input.contains(&ModelInputModality::Image) {
            println!("Skipping tool text+image result test - model {} doesn't support images", model.id);
            return;
        }
        let text = image_tool_round_trip(
            model,
            options,
            "get_circle_with_description",
            "Returns a circle image with a text description",
            "Use the get_circle_with_description tool and tell me what you learned. Also say what color the shape is.",
            Some("This is a geometric shape with specific properties: it has a diameter of 100 pixels."),
        )
        .await;
        assert!(regex::Regex::new("diameter|100|pixel").unwrap().is_match(&text), "{text}");
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn image_tool_result_e2e() {
    let targets = vec![
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
        target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5-mini"),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
        target("Anthropic", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-haiku-4-5"),
        target("OpenRouter", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "z-ai/glm-4.5v"),
        target("Mistral", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "pixtral-12b"),
        target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3").extra("reasoningEffort", json!("high")),
        target("Xiaomi MiMo (API billing)", Gate::Env(&["XIAOMI_API_KEY"]), "xiaomi", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (CN)", Gate::Env(&["XIAOMI_TOKEN_PLAN_CN_API_KEY"]), "xiaomi-token-plan-cn", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (AMS)", Gate::Env(&["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]), "xiaomi-token-plan-ams", "mimo-v2.6-pro"),
        target("Xiaomi MiMo Token Plan (SGP)", Gate::Env(&["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]), "xiaomi-token-plan-sgp", "mimo-v2.6-pro"),
        target("Qwen Token Plan", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan", "qwen3.7-max"),
        target("Qwen Token Plan Individual", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan-individual", "qwen3.8-max"),
        target("Qwen Token Plan (CN)", Gate::Env(&["QWEN_TOKEN_PLAN_CN_API_KEY"]), "qwen-token-plan-cn", "qwen3.7-max"),
        target("Kimi For Coding", Gate::Env(&["KIMI_API_KEY"]), "kimi-coding", "kimi-for-coding"),
        target("Vercel AI Gateway", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "google/gemini-2.5-flash"),
        target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"),
        target("Anthropic OAuth", Gate::OAuth("anthropic"), "anthropic", "claude-sonnet-4-5"),
        target("GitHub Copilot claude-haiku-4.5", Gate::OAuth("github-copilot"), "github-copilot", "claude-haiku-4.5"),
        target("GitHub Copilot claude-sonnet-4.6", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
    ];
    run_matrix(
        "image-tool-result.test.ts",
        targets,
        &[
            ("should handle tool result with only image", tool_with_image_result),
            ("should handle tool result with text and image", tool_with_text_and_image_result),
        ],
        3,
    )
    .await;
}

// ---------------------------------------------------------------------------
// total-tokens.test.ts
// ---------------------------------------------------------------------------

fn long_system_prompt() -> String {
    let filler = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.";
    format!(
        "You are a helpful assistant. Be concise in your responses.\n\nHere is some additional context that makes this system prompt long enough to trigger caching:\n\n{}\n\nRemember: Always be helpful and concise.",
        vec![filler; 50].join("\n\n")
    )
}

async fn total_tokens_with_cache(model: &Model, options: StreamOptions) -> (Usage, Usage) {
    let first_user = json!({"role": "user", "content": "What is 2 + 2? Reply with just the number.", "timestamp": now()});
    let context = ctx(json!({"systemPrompt": long_system_prompt(), "messages": [first_user]}));
    let first = complete(model, &context, options.clone()).await;
    assert_eq!(first.stop_reason, StopReason::Stop, "{:?}", first.error_message);
    let mut second_context = context.clone();
    second_context.messages.push(Message::Assistant(first.clone()));
    second_context.messages.push(user_string("What is 3 + 3? Reply with just the number."));
    let second = complete(model, &second_context, options).await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{:?}", second.error_message);
    for (label, usage) in [("First request", &first.usage), ("Second request", &second.usage)] {
        println!(
            "  {label}: input {}, output {}, cacheRead {}, cacheWrite {}, totalTokens {}",
            usage.input, usage.output, usage.cache_read, usage.cache_write, usage.total_tokens
        );
    }
    (first.usage, second.usage)
}

#[track_caller]
fn assert_total_is_sum(usage: &Usage) {
    assert_eq!(usage.total_tokens, usage.input + usage.output + usage.cache_read + usage.cache_write);
}

fn total_tokens_equal_components(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let (first, second) = total_tokens_with_cache(model, options).await;
        assert_total_is_sum(&first);
        assert_total_is_sum(&second);
        // Anthropic should report cache activity.
        if model.provider == "anthropic" {
            assert!(second.cache_read > 0 || second.cache_write > 0 || first.cache_write > 0);
        }
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn total_tokens_e2e() {
    let high = |t: Target| t.extra("reasoningEffort", json!("high"));
    let targets = vec![
        target("Anthropic (API Key)", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-sonnet-4-5"),
        target("Anthropic (OAuth)", Gate::OAuth("anthropic"), "anthropic", "claude-sonnet-4-6"),
        target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
        target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o"),
        target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
        target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
        target("xAI", Gate::Env(&["XAI_API_KEY"]), "xai", "grok-4.3"),
        target("Groq", Gate::Env(&["GROQ_API_KEY"]), "groq", "openai/gpt-oss-120b"),
        target("Cerebras", Gate::Env(&["CEREBRAS_API_KEY"]), "cerebras", "gpt-oss-120b"),
        target("Cloudflare Workers AI", Gate::CloudflareWorkers, "cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        target("Cloudflare AI Gateway", Gate::CloudflareGateway, "cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
        target("Hugging Face", Gate::Env(&["HF_TOKEN"]), "huggingface", "moonshotai/Kimi-K2.5"),
        high(target("Together AI", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3")),
        high(target("Baseten", Gate::Env(&["BASETEN_API_KEY"]), "baseten", "zai-org/GLM-5.2")),
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
        target("OpenRouter deepseek/deepseek-chat", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "deepseek/deepseek-chat"),
        target("OpenRouter mistralai/mistral-small-3.2-24b-instruct", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "mistralai/mistral-small-3.2-24b-instruct"),
        target("OpenRouter google/gemini-2.5-flash", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "google/gemini-2.5-flash"),
        target("OpenRouter deepseek/deepseek-chat (repeat)", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "deepseek/deepseek-chat"),
        target("GitHub Copilot claude-haiku-4.5", Gate::OAuth("github-copilot"), "github-copilot", "claude-haiku-4.5"),
        target("GitHub Copilot claude-sonnet-4.6", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
        target("Amazon Bedrock", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"),
        target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
    ];
    run_matrix(
        "total-tokens.test.ts",
        targets,
        &[("should return totalTokens equal to sum of components", total_tokens_equal_components)],
        3,
    )
    .await;
}
