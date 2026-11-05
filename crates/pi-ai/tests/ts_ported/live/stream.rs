//! Live port of the "Generate E2E Tests" provider matrix in stream.test.ts.

use pi_ai::compat::{complete, stream};
use pi_ai::*;
use serde_json::{Value, json};

use super::matrix::red_circle_base64;
use super::*;
use crate::support::ctx;

fn calculator_tool() -> Value {
    json!({"name": "math_operation", "description": "Perform basic arithmetic operations", "parameters": {"type": "object", "properties": {
        "a": {"type": "number", "description": "First number"},
        "b": {"type": "number", "description": "Second number"},
        "operation": {"type": "string", "enum": ["add", "subtract", "multiply", "divide"],
            "description": "The operation to perform. One of 'add', 'subtract', 'multiply', 'divide'."}
    }, "required": ["a", "b", "operation"]}})
}

#[track_caller]
fn expect_usage(message: &AssistantMessage) {
    assert!(message.usage.input + message.usage.cache_read > 0, "{:?}", message.usage);
    assert!(message.usage.output > 0, "{:?}", message.usage);
    assert!(message.error_message.as_deref().unwrap_or_default().is_empty(), "{:?}", message.error_message);
}

fn basic_text_generation(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let mut context = ctx(json!({"systemPrompt": "You are a helpful assistant. Be concise.",
            "messages": [{"role": "user", "content": "Reply with exactly: 'Hello test successful'", "timestamp": now()}]}));
        let first = complete(model, &context, options.clone()).await;
        expect_usage(&first);
        assert!(text_of(&first).contains("Hello test successful"), "{}", text_of(&first));
        context.messages.push(Message::Assistant(first));
        context.messages.push(user_string("Now say 'Goodbye test successful'"));
        let second = complete(model, &context, options).await;
        expect_usage(&second);
        assert!(text_of(&second).contains("Goodbye test successful"), "{}", text_of(&second));
    })
}

fn partial_tool_call(partial: &AssistantMessage, index: usize) -> &ToolCall {
    match &partial.content[index] {
        AssistantContent::ToolCall(call) => call,
        other => panic!("expected a tool call at {index}: {other:?}"),
    }
}

fn handle_tool_call(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant that uses tools when asked.",
            "messages": [{"role": "user", "content": "Calculate 15 + 27 using the math_operation tool.", "timestamp": now()}],
            "tools": [calculator_tool()]}));
        let (events, response) = stream_events(stream(model, &context, options)).await;
        let (mut started, mut delta, mut ended) = (false, false, false);
        let mut accumulated = String::new();
        let mut index = 0;
        for event in &events {
            match event {
                AssistantMessageEvent::ToolCallStart { content_index, partial } => {
                    started = true;
                    index = *content_index;
                    let call = partial_tool_call(partial, index);
                    assert_eq!(call.name, "math_operation");
                    assert!(!call.id.is_empty());
                }
                AssistantMessageEvent::ToolCallDelta { content_index, delta: chunk, partial } => {
                    delta = true;
                    assert_eq!(*content_index, index);
                    assert_eq!(partial_tool_call(partial, index).name, "math_operation");
                    accumulated.push_str(chunk);
                }
                AssistantMessageEvent::ToolCallEnd { content_index, partial, .. } => {
                    ended = true;
                    assert_eq!(*content_index, index);
                    let call = partial_tool_call(partial, index);
                    assert_eq!(call.name, "math_operation");
                    serde_json::from_str::<Value>(&accumulated).expect("streamed arguments are JSON");
                    assert_eq!(call.arguments["a"].as_f64(), Some(15.0));
                    assert_eq!(call.arguments["b"].as_f64(), Some(27.0));
                    assert!(["add", "subtract", "multiply", "divide"].contains(&call.arguments["operation"].as_str().unwrap_or_default()));
                }
                _ => {}
            }
        }
        assert!(started && delta && ended, "start {started} delta {delta} end {ended}");
        assert_eq!(response.stop_reason, StopReason::ToolUse, "{:?}", response.error_message);
        let call = response.tool_calls().next().expect("No tool call found in response");
        assert_eq!(call.name, "math_operation");
        assert!(!call.id.is_empty());
    })
}

fn handle_streaming(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant.", "messages": [{"role": "user", "content": "Count from 1 to 3", "timestamp": now()}]}));
        let (events, response) = stream_events(stream(model, &context, options)).await;
        let started = events.iter().any(|e| matches!(e, AssistantMessageEvent::TextStart { .. }));
        let chunks: String = events
            .iter()
            .filter_map(|e| match e {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        let completed = events.iter().any(|e| matches!(e, AssistantMessageEvent::TextEnd { .. }));
        assert!(started && !chunks.is_empty() && completed, "{:?}", response.error_message);
        assert!(response.content.iter().any(|b| matches!(b, AssistantContent::Text(_))));
    })
}

fn handle_thinking(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let n = now() % 255;
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant.", "messages": [{"role": "user",
            "content": format!("Think long and hard about {n} + 27. Think step by step. Then output the result."), "timestamp": now()}]}));
        let (events, response) = stream_events(stream(model, &context, options)).await;
        assert_eq!(response.stop_reason, StopReason::Stop, "Error: {:?}", response.error_message);
        let started = events.iter().any(|e| matches!(e, AssistantMessageEvent::ThinkingStart { .. }));
        let chunks: usize = events
            .iter()
            .map(|e| match e {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => delta.len(),
                _ => 0,
            })
            .sum();
        let completed = events.iter().any(|e| matches!(e, AssistantMessageEvent::ThinkingEnd { .. }));
        assert!(started && chunks > 0 && completed);
        assert!(has_thinking(&response));
    })
}

fn handle_image(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        if !model.input.contains(&ModelInputModality::Image) {
            println!("Skipping image test - model {} doesn't support images", model.id);
            return;
        }
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant.", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "What do you see in this image? Please describe the shape (circle, rectangle, square, triangle, ...) and color (red, blue, green, ...). You MUST reply in English."},
            {"type": "image", "data": red_circle_base64(), "mimeType": "image/png"}
        ], "timestamp": now()}]}));
        let response = complete(model, &context, options).await;
        assert!(!response.content.is_empty(), "{:?}", response.error_message);
        if let Some(AssistantContent::Text(text)) = response.content.iter().find(|b| matches!(b, AssistantContent::Text(_))) {
            let lower = text.text.to_lowercase();
            assert!(lower.contains("red") && lower.contains("circle"), "{lower}");
        }
    })
}

fn multi_turn(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let mut context = ctx(json!({"systemPrompt": "You are a helpful assistant that can use tools to answer questions.",
            "messages": [{"role": "user", "content": "Think about this briefly, then calculate 42 * 17 and 453 + 434 using the math_operation tool.", "timestamp": now()}],
            "tools": [calculator_tool()]}));
        let mut text = String::new();
        let (mut saw_thinking, mut saw_tools) = (false, false);
        for _ in 0..5 {
            let response = complete(model, &context, options.clone()).await;
            let mut results = Vec::new();
            for block in &response.content {
                match block {
                    AssistantContent::Text(t) => text.push_str(&t.text),
                    AssistantContent::Thinking(_) => saw_thinking = true,
                    AssistantContent::ToolCall(call) => {
                        saw_tools = true;
                        assert_eq!(call.name, "math_operation");
                        assert!(!call.id.is_empty());
                        let (a, b) = (call.arguments["a"].as_f64().expect("Invalid math arguments"), call.arguments["b"].as_f64().expect("Invalid math arguments"));
                        let result = match call.arguments["operation"].as_str() {
                            Some("add") => a + b,
                            Some("multiply") => a * b,
                            _ => 0.0,
                        };
                        results.push(json!({"role": "toolResult", "toolCallId": call.id, "toolName": call.name,
                            "content": [{"type": "text", "text": format!("{result}")}], "isError": false, "timestamp": now()}));
                    }
                }
            }
            let stop = response.stop_reason;
            let error = response.error_message.clone();
            context.messages.push(Message::Assistant(response));
            for result in results {
                context.messages.push(serde_json::from_value(result).unwrap());
            }
            assert_ne!(stop, StopReason::Error, "Error: {error:?}");
            if stop == StopReason::Stop {
                break;
            }
        }
        assert!(saw_thinking || saw_tools);
        assert!(text.contains("714") && text.contains("887"), "{text}");
    })
}

struct It {
    title: &'static str,
    scenario: Scenario,
    extra: Value,
    reasoning: Option<&'static str>,
    model: Option<(&'static str, &'static str)>,
    websocket: bool,
    authorization: Option<String>,
}

fn it(title: &'static str, scenario: Scenario) -> It {
    It { title, scenario, extra: json!({}), reasoning: None, model: None, websocket: false, authorization: None }
}

impl It {
    fn with(mut self, extra: Value) -> Self {
        self.extra.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self
    }
    fn reason(mut self, level: &'static str) -> Self {
        self.reasoning = Some(level);
        self
    }
    fn on(mut self, provider: &'static str, id: &'static str) -> Self {
        self.model = Some((provider, id));
        self
    }
    fn ws(mut self) -> Self {
        self.websocket = true;
        self
    }
    /// BYOK: `headers: { Authorization: "Bearer <upstream key>" }`.
    fn upstream_key(mut self, env_name: &str) -> Self {
        self.authorization = Some(format!("Bearer {}", env(env_name).unwrap_or_default()));
        self
    }
}

fn basic() -> It {
    it("should complete basic text generation", basic_text_generation)
}
fn tools() -> It {
    it("should handle tool calling", handle_tool_call)
}
fn streaming() -> It {
    it("should handle streaming", handle_streaming)
}
fn thinking() -> It {
    it("should handle thinking", handle_thinking)
}
fn image() -> It {
    it("should handle image input", handle_image)
}
fn turns() -> It {
    it("should handle multi-turn with thinking and tools", multi_turn)
}
fn with_image(mut its: Vec<It>) -> Vec<It> {
    its.push(image());
    its
}

fn effort(level: &str) -> Value {
    json!({"reasoningEffort": level})
}

/// basic, tools, streaming, then thinking and multi-turn with the same extra options.
fn standard(extra: Value) -> Vec<It> {
    vec![basic(), tools(), streaming(), thinking().with(extra.clone()), turns().with(extra)]
}

async fn run_blocks(file: &str, blocks: Vec<(Target, Vec<It>)>) {
    let mut failures = Vec::new();
    for (target, its) in blocks {
        let Some((base_model, base_options)) = resolve(&target).await else {
            println!("[{file}] skip {} ({:?} not configured)", target.label, target.gate);
            continue;
        };
        for case in its {
            let model = match case.model {
                Some((provider, id)) => catalog_model(provider, id),
                None => base_model.clone(),
            };
            let mut options = base_options.clone();
            options.extra.extend(case.extra.as_object().unwrap().clone());
            if let Some(level) = case.reasoning {
                options.reasoning = Some(serde_json::from_value(json!(level)).unwrap());
            }
            if case.websocket {
                options.transport = Some(Transport::Websocket);
            }
            if let Some(authorization) = case.authorization.clone() {
                let mut headers = ProviderHeaders::new();
                headers.insert("Authorization".into(), Some(authorization));
                options.headers = Some(headers);
            }
            let scenario = case.scenario;
            let result = attempt(3, move || {
                let model = model.clone();
                let options = options.clone();
                Box::pin(async move { scenario(&model, options).await })
            })
            .await;
            match result {
                Ok(()) => println!("[{file}] ok   {} :: {}", target.label, case.title),
                Err(error) => failures.push(format!("{} :: {}: {error}", target.label, case.title)),
            }
        }
    }
    assert!(failures.is_empty(), "{file}: {} failure(s):\n{}", failures.len(), failures.join("\n"));
}

fn vertex_target(label: &str) -> Target {
    let project = env("GOOGLE_CLOUD_PROJECT").or_else(|| env("GCLOUD_PROJECT"));
    let gate = if project.is_some() { Gate::Env(&["GOOGLE_CLOUD_LOCATION"]) } else { Gate::Env(&["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"]) };
    target(label, gate, "google-vertex", "gemini-3-flash-preview")
        .extra("project", json!(project))
        .extra("location", json!(env("GOOGLE_CLOUD_LOCATION")))
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn generate_e2e() {
    let thinking_high = json!({"thinkingEnabled": true, "reasoningEffort": "high"});
    let budget_2048 = json!({"thinkingEnabled": true, "thinkingBudgetTokens": 2048});
    let mut openai_byok = target("Cloudflare AI Gateway → OpenAI BYOK (gpt-5.1)", Gate::CloudflareGateway, "cloudflare-ai-gateway", "gpt-5.1");
    if env("OPENAI_API_KEY").is_none() {
        openai_byok.gate = Gate::Env(&["OPENAI_API_KEY"]);
    }
    let mut anthropic_byok = target("Cloudflare AI Gateway → Anthropic BYOK (claude-sonnet-4-5)", Gate::CloudflareGateway, "cloudflare-ai-gateway", "claude-sonnet-4-5");
    if env("ANTHROPIC_API_KEY").is_none() {
        anthropic_byok.gate = Gate::Env(&["ANTHROPIC_API_KEY"]);
    }
    let blocks: Vec<(Target, Vec<It>)> = vec![
        (target("Gemini (gemini-2.5-flash)", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"), vec![
            basic(), tools(), streaming(),
            thinking().with(json!({"thinking": {"enabled": true, "budgetTokens": 1024}})),
            turns().with(json!({"thinking": {"enabled": true, "budgetTokens": 2048}})),
            image(),
        ]),
        (vertex_target("Google Vertex (gemini-3-flash-preview)"), vec![
            basic(), tools(),
            thinking().with(json!({"thinking": {"enabled": true, "budgetTokens": 1024, "level": "LOW"}})),
            streaming(),
            turns().with(json!({"thinking": {"enabled": true, "budgetTokens": 1024, "level": "MEDIUM"}})),
            image(),
        ]),
        (target("Google Vertex (API key)", Gate::Env(&["GOOGLE_CLOUD_API_KEY"]), "google-vertex", "gemini-3-flash-preview")
            .api_key(&env("GOOGLE_CLOUD_API_KEY").unwrap_or_default()),
            vec![it("should complete basic text generation with Vertex API key", basic_text_generation)]),
        (target("OpenAI Completions (gpt-4o-mini)", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
            vec![basic(), tools(), streaming(), image()]),
        (target("DeepSeek (deepseek-flash)", Gate::Env(&["DEEPSEEK_API_KEY"]), "deepseek", "deepseek-flash"), standard(effort("high"))),
        (target("OpenAI Responses (gpt-5.4)", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5.4"),
            with_image(standard(effort("high")))),
        (target("Anthropic (claude-haiku-4-5)", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-haiku-4-5"),
            vec![basic().with(json!({"thinkingEnabled": true})), tools(), streaming(), image()]),
        (target("Azure OpenAI Responses (gpt-4o-mini)", Gate::Azure, "azure", "gpt-4o-mini"), vec![basic(), tools(), streaming(), image()]),
        (target("xAI (grok-4.7)", Gate::Env(&["XAI_API_KEY"]), "xai", "grok-4.7"), standard(effort("medium"))),
        (target("Groq (gpt-oss-20b)", Gate::Env(&["GROQ_API_KEY"]), "groq", "openai/gpt-oss-20b"), standard(effort("medium"))),
        (target("Cerebras (gpt-oss-120b)", Gate::Env(&["CEREBRAS_API_KEY"]), "cerebras", "gpt-oss-120b"), standard(effort("medium"))),
        (target("Cloudflare Workers AI (Kimi K2.6)", Gate::CloudflareWorkers, "cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"), standard(effort("medium"))),
        (target("Cloudflare AI Gateway → Workers AI (Kimi K2.6)", Gate::CloudflareGateway, "cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
            standard(effort("medium"))),
        (openai_byok, standard(json!({"thinkingEnabled": true, "reasoningEffort": "medium"})).into_iter().map(|i| i.upstream_key("OPENAI_API_KEY")).collect()),
        (anthropic_byok, standard(thinking_high.clone()).into_iter().map(|i| i.upstream_key("ANTHROPIC_API_KEY")).collect()),
        (target("Hugging Face (Kimi-K2.5)", Gate::Env(&["HF_TOKEN"]), "huggingface", "moonshotai/Kimi-K2.5"), standard(effort("medium"))),
        (target("Together AI (Kimi-K3)", Gate::Env(&["TOGETHER_API_KEY"]), "together", "moonshotai/Kimi-K3"),
            with_image(standard(effort("high")))),
        (target("Baseten (GLM 5.2)", Gate::Env(&["BASETEN_API_KEY"]), "baseten", "zai-org/GLM-5.2").extra("reasoningEffort", json!("high")), standard(json!({}))),
        (target("NVIDIA NIM (Nemotron 3.5 Super VL)", Gate::Env(&["NVIDIA_API_KEY"]), "nvidia", "nvidia/nemotron-3.5-super-vl-120b-a12b"), standard(effort("high"))),
        (target("OpenRouter (glm-4.5v)", Gate::Env(&["OPENROUTER_API_KEY"]), "openrouter", "z-ai/glm-4.5v"),
            with_image(standard(effort("medium")))),
        (target("Vercel AI Gateway (google/gemini-2.5-flash)", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "google/gemini-2.5-flash"),
            vec![basic(), tools(), streaming(), image(), it("should handle multi-turn with tools", multi_turn)]),
        (target("Vercel AI Gateway (anthropic/claude-opus-4.5)", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "anthropic/claude-opus-4.5"),
            vec![basic(), tools(), streaming(), image(), it("should handle multi-turn with tools", multi_turn)]),
        (target("Vercel AI Gateway (openai/gpt-5.1-codex-max)", Gate::Env(&["AI_GATEWAY_API_KEY"]), "vercel-ai-gateway", "openai/gpt-5.1-codex-max"),
            vec![basic(), tools(), streaming(), image(), it("should handle multi-turn with tools", multi_turn)]),
        (target("zAI (glm-5.2)", Gate::Env(&["ZAI_API_KEY"]), "zai", "glm-5.2"), with_image(standard(effort("medium")))),
        (target("Mistral (devstral-medium-latest)", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "devstral-medium-latest"), vec![
            basic(), tools(), streaming(),
            thinking().with(effort("high")).on("mistral", "mistral-small-2603"),
            turns().with(effort("high")).on("mistral", "mistral-small-2603"),
        ]),
        (target("Mistral (pixtral-12b)", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "pixtral-12b"), vec![basic(), tools(), streaming(), image()]),
        (target("MiniMax (MiniMax-M2.7)", Gate::Env(&["MINIMAX_API_KEY"]), "minimax", "MiniMax-M2.7"), standard(budget_2048.clone())),
        (target("Kimi For Coding (kimi-for-coding)", Gate::Env(&["KIMI_API_KEY"]), "kimi-coding", "kimi-for-coding"), standard(budget_2048.clone())),
        (target("Meta (muse-spark-1.3)", Gate::Env(&["META_API_KEY"]), "meta", "muse-spark-1.3"), standard(budget_2048)),
        (target("Xiaomi MiMo (API billing)", Gate::Env(&["XIAOMI_API_KEY"]), "xiaomi", "mimo-v2.6-pro"), standard(thinking_high.clone())),
        (target("Xiaomi MiMo Token Plan (CN)", Gate::Env(&["XIAOMI_TOKEN_PLAN_CN_API_KEY"]), "xiaomi-token-plan-cn", "mimo-v2.6-pro"), standard(thinking_high.clone())),
        (target("Xiaomi MiMo Token Plan (AMS)", Gate::Env(&["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]), "xiaomi-token-plan-ams", "mimo-v2.6-pro"), standard(thinking_high.clone())),
        (target("Xiaomi MiMo Token Plan (SGP)", Gate::Env(&["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]), "xiaomi-token-plan-sgp", "mimo-v2.6-pro"), standard(thinking_high.clone())),
        (target("Qwen Token Plan (Qwen3.7-Max)", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan", "qwen3.7-max"), standard(thinking_high.clone())),
        (target("Qwen Token Plan Individual (Qwen3.8-Max)", Gate::Env(&["QWEN_TOKEN_PLAN_API_KEY"]), "qwen-token-plan-individual", "qwen3.8-max"), standard(thinking_high.clone())),
        (target("Qwen Token Plan (CN)", Gate::Env(&["QWEN_TOKEN_PLAN_CN_API_KEY"]), "qwen-token-plan-cn", "qwen3.7-max"), standard(thinking_high)),
        (target("Ant Ling (Ling 2.6 Flash)", Gate::Env(&["ANT_LING_API_KEY"]), "ant-ling", "Ling-2.6-flash"), vec![
            basic(), tools(), streaming(), thinking().with(effort("high")).on("ant-ling", "Ring-2.6-1T"),
        ]),
        (target("Anthropic OAuth (claude-sonnet-4-6)", Gate::OAuth("anthropic"), "anthropic", "claude-sonnet-4-6"), vec![
            basic(), tools(), streaming(), thinking().with(json!({"thinkingEnabled": true})), turns().with(json!({"thinkingEnabled": true})), image(),
        ]),
        (target("Anthropic OAuth (claude-opus-4-6 adaptive)", Gate::OAuth("anthropic"), "anthropic", "claude-opus-4-6"), vec![
            basic(), tools(), streaming(),
            it("should handle adaptive thinking with effort high", handle_thinking).with(json!({"thinkingEnabled": true, "effort": "high"})),
            it("should handle adaptive thinking with effort medium", handle_thinking).with(json!({"thinkingEnabled": true, "effort": "medium"})),
            it("should handle multi-turn with adaptive thinking and tools", multi_turn).with(json!({"thinkingEnabled": true, "effort": "high"})),
            image(),
        ]),
        (target("GitHub Copilot (gpt-6-luna)", Gate::OAuth("github-copilot"), "github-copilot", "gpt-6-luna"),
            with_image(standard(effort("high")))),
        (target("GitHub Copilot (claude-sonnet-4.6)", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
            with_image(standard(json!({"thinkingEnabled": true})))),
        (target("OpenAI Codex (gpt-6.1-sol)", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"), vec![
            basic(), tools(), streaming(), it("should handle thinking with reasoningEffort xhigh", handle_thinking).with(effort("xhigh")),
            turns().with(effort("xhigh")), image(),
        ]),
        (target("OpenAI Codex (gpt-6.1-sol via WebSocket)", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"), vec![
            basic().ws(), tools().ws(), streaming().ws(), it("should handle thinking with reasoningEffort xhigh", handle_thinking).with(effort("xhigh")).ws(),
            turns().with(effort("xhigh")).ws(), image().ws(),
        ]),
        (target("Amazon Bedrock (claude-sonnet-4-5)", Gate::Bedrock, "amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"), vec![
            basic(), tools(), streaming(), thinking().reason("medium"), turns().reason("high"), image(),
        ]),
        (target("Ollama (gpt-oss-20b)", Gate::Ollama, "ollama", "local")
            .custom(json!({"id": "gpt-oss:20b", "api": "openai-completions", "provider": "ollama", "baseUrl": "http://localhost:11434/v1", "reasoning": true,
                "input": ["text"], "contextWindow": 128000, "maxTokens": 16000, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "name": "Ollama GPT-OSS 20B"}))
            .api_key("test"),
            vec![basic(), tools(), streaming(), it("should handle thinking mode", handle_thinking).with(effort("medium")), turns().with(effort("medium"))]),
    ];
    run_blocks("stream.test.ts", blocks).await;
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn bedrock_interleaved_and_request_metadata_e2e() {
    const FILE: &str = "stream.test.ts";
    run_case(FILE, "Bedrock: should use adaptive thinking without anthropic_beta", &[Gate::Bedrock], 3, |_| {
        Box::pin(async {
            let (hook, payload) = payload_recorder();
            let mut options = StreamOptions { reasoning: Some(serde_json::from_value(json!("xhigh")).unwrap()), on_payload: Some(hook), ..Default::default() };
            options.extra.insert("interleavedThinking".into(), json!(true));
            let context = ctx(json!({"systemPrompt": "You are a helpful assistant that uses tools when asked.",
                "messages": [{"role": "user", "content": "Think first, then calculate 15 + 27 using the math_operation tool.", "timestamp": now()}],
                "tools": [calculator_tool()]}));
            let response = complete(&catalog_model("amazon-bedrock", "global.anthropic.claude-opus-4-6-v1"), &context, options).await;
            assert_ne!(response.stop_reason, StopReason::Error, "Error: {:?}", response.error_message);
            let payload = payload.lock().clone().expect("payload");
            let fields = &payload["additionalModelRequestFields"];
            assert_eq!(fields["thinking"], json!({"type": "adaptive", "display": "summarized"}));
            assert_eq!(fields["output_config"], json!({"effort": "max"}));
            assert!(fields.get("anthropic_beta").is_none());
        })
    })
    .await;
    for with_metadata in [true, false] {
        let name = if with_metadata { "Bedrock: should pass requestMetadata to the SDK payload" } else { "Bedrock: should omit requestMetadata from payload when not provided" };
        let case: fn(Vec<Option<String>>) -> CaseFuture = if with_metadata {
            |_| Box::pin(request_metadata_case(true))
        } else {
            |_| Box::pin(request_metadata_case(false))
        };
        run_case(FILE, name, &[Gate::Bedrock], 3, case).await;
    }
}

async fn request_metadata_case(with_metadata: bool) {
    let (hook, payload) = payload_recorder();
    let mut options = StreamOptions { on_payload: Some(hook), ..Default::default() };
    let metadata = json!({"app": "pi-test", "env": "ci"});
    if with_metadata {
        options.extra.insert("requestMetadata".into(), metadata.clone());
    }
    let context = ctx(json!({"messages": [{"role": "user", "content": "Say hi.", "timestamp": now()}]}));
    let response = complete(&catalog_model("amazon-bedrock", "global.anthropic.claude-sonnet-4-5-20250929-v1:0"), &context, options).await;
    assert_ne!(response.stop_reason, StopReason::Error, "Error: {:?}", response.error_message);
    let payload = payload.lock().clone().expect("payload");
    if with_metadata {
        assert_eq!(payload["requestMetadata"], metadata);
    } else {
        assert!(payload.get("requestMetadata").is_none());
    }
}
