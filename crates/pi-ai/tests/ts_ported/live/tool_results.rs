//! Live ports of openai-responses-tool-result-images.test.ts,
//! anthropic-tool-name-normalization.test.ts and responseid.test.ts.

use pi_ai::compat::{complete, stream};
use pi_ai::*;
use serde_json::json;

use super::matrix::red_circle_base64;
use super::*;
use crate::support::ctx;

// ---------------------------------------------------------------------------
// openai-responses-tool-result-images.test.ts
// ---------------------------------------------------------------------------

fn tool_result_images_stay_in_function_call_output(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        if !model.input.contains(&ModelInputModality::Image) {
            println!("Skipping responses tool-result image test. Model {} does not support images.", model.id);
            return;
        }
        let tool_text = "A red circle with a diameter of 100 pixels.";
        let mut context = ctx(json!({
            "systemPrompt": "You are a helpful assistant that always uses the provided tool when asked.",
            "messages": [{"role": "user", "content": "Call get_circle_with_description, then describe both the tool text and the image. Mention the color and shape.", "timestamp": now()}],
            "tools": [{"name": "get_circle_with_description", "description": "Returns a red circle image with a short text description.", "parameters": {"type": "object", "properties": {}}}]
        }));
        let first = complete(model, &context, options.clone()).await;
        assert_eq!(first.stop_reason, StopReason::ToolUse, "Error: {:?}", first.error_message);
        let call = first.tool_calls().next().expect("tool call").clone();
        context.messages.push(Message::Assistant(first));
        context.messages.push(
            serde_json::from_value(json!({"role": "toolResult", "toolCallId": call.id, "toolName": call.name, "content": [
                {"type": "text", "text": tool_text}, {"type": "image", "data": red_circle_base64(), "mimeType": "image/png"}
            ], "isError": false, "timestamp": now()}))
            .unwrap(),
        );
        let (hook, payload) = payload_recorder();
        let second = complete(model, &context, StreamOptions { on_payload: Some(hook), ..options }).await;
        assert_eq!(second.stop_reason, StopReason::Stop, "Error: {:?}", second.error_message);
        assert!(second.error_message.as_deref().unwrap_or_default().is_empty());
        let payload = payload.lock().clone().expect("payload");
        let input = payload["input"].as_array().expect("input array");
        let index = input.iter().position(|i| i["type"] == "function_call_output").expect("function_call_output item");
        let output = input[index]["output"].as_array().expect("function_call_output output content array");
        let text = output.iter().find(|i| i["type"] == "input_text").expect("input_text");
        let image = output.iter().find(|i| i["type"] == "input_image").expect("input_image");
        assert!(text["text"].as_str().unwrap().contains(tool_text));
        assert!(image["image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(input[index + 1..].iter().all(|i| i["role"] != "user"), "images must not move to a later user message");
        let reply = text_of(&second).to_lowercase();
        assert!(reply.contains("red") && reply.contains("circle"), "{reply}");
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn openai_responses_tool_result_images_e2e() {
    let low = |t: Target| t.extra("reasoningEffort", json!("low"));
    run_matrix(
        "openai-responses-tool-result-images.test.ts",
        vec![
            low(target("OpenAI Responses (gpt-5-mini)", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5-mini")),
            target("Azure OpenAI Responses (gpt-4o-mini)", Gate::Azure, "azure", "gpt-4o-mini"),
            low(target("GitHub Copilot Responses (gpt-6-luna)", Gate::OAuth("github-copilot"), "github-copilot", "gpt-6-luna")),
            low(target("OpenAI Codex Responses (gpt-6.1-sol)", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol")),
        ],
        &[("should send tool result images in function_call_output", tool_result_images_stay_in_function_call_output)],
        3,
    )
    .await;
}

// ---------------------------------------------------------------------------
// anthropic-tool-name-normalization.test.ts
// ---------------------------------------------------------------------------

async fn tool_call_name(tool: &str, description: &str, property: &str, property_description: &str, system: &str, ask: &str, key: Option<String>) -> String {
    let model = catalog_model("anthropic", "claude-sonnet-4-6");
    let context = ctx(json!({"systemPrompt": system, "messages": [{"role": "user", "content": ask, "timestamp": now()}],
        "tools": [{"name": tool, "description": description, "parameters": {"type": "object",
            "properties": {property: {"type": "string", "description": property_description}}, "required": [property]}}]}));
    let (events, response) = stream_events(stream(&model, &context, StreamOptions { api_key: key, ..Default::default() })).await;
    assert_eq!(response.stop_reason, StopReason::ToolUse, "Error: {:?}", response.error_message);
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.name.clone()),
            _ => None,
        })
        .expect("toolcall_end")
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn anthropic_oauth_tool_name_normalization_e2e() {
    const FILE: &str = "anthropic-tool-name-normalization.test.ts";
    let gate = [Gate::OAuth("anthropic")];
    run_case(FILE, "todowrite -> TodoWrite -> todowrite", &gate, 0, |keys| {
        Box::pin(async move {
            let name = tool_call_name("todowrite", "Write a todo item", "task", "The task to add",
                "You are a helpful assistant. Use the todowrite tool when asked to add todos.", "Add a todo: buy milk. Use the todowrite tool.", keys[0].clone()).await;
            assert_eq!(name, "todowrite");
        })
    })
    .await;
    run_case(FILE, "pi's built-in tools (read)", &gate, 0, |keys| {
        Box::pin(async move {
            let name = tool_call_name("read", "Read a file", "path", "File path", "You are a helpful assistant. Use the read tool to read files.",
                "Read the file /tmp/test.txt using the read tool.", keys[0].clone()).await;
            assert_eq!(name, "read");
        })
    })
    .await;
    run_case(FILE, "find is not mapped to Glob", &gate, 0, |keys| {
        Box::pin(async move {
            let name = tool_call_name("find", "Find files by pattern", "pattern", "Glob pattern", "You are a helpful assistant. Use the find tool to search for files.",
                "Find all .ts files using the find tool.", keys[0].clone()).await;
            assert_eq!(name, "find");
        })
    })
    .await;
    run_case(FILE, "custom tools that don't match any CC tool names", &gate, 0, |keys| {
        Box::pin(async move {
            let name = tool_call_name("my_custom_tool", "A custom tool", "input", "Input value", "You are a helpful assistant. Use my_custom_tool when asked.",
                "Use my_custom_tool with input 'hello'.", keys[0].clone()).await;
            assert_eq!(name, "my_custom_tool");
        })
    })
    .await;
}

// ---------------------------------------------------------------------------
// responseid.test.ts
// ---------------------------------------------------------------------------

fn expect_response_id(model: &Model, options: StreamOptions) -> ScenarioFuture<'_> {
    Box::pin(async move {
        let context = ctx(json!({"systemPrompt": "You are a helpful assistant. Be concise.",
            "messages": [{"role": "user", "content": "Reply with exactly: response id test", "timestamp": now()}]}));
        let response = complete(model, &context, options).await;
        assert_ne!(response.stop_reason, StopReason::Error, "{:?}", response.error_message);
        assert!(response.response_id.as_deref().is_some_and(|id| !id.is_empty()));
    })
}

#[tokio::test]
#[ignore = "live: needs provider credentials"]
async fn response_id_e2e() {
    let mut vertex_adc = target("Google Vertex (ADC)", Gate::Env(&["GOOGLE_CLOUD_LOCATION"]), "google-vertex", "gemini-3-flash-preview");
    match env("GOOGLE_CLOUD_PROJECT").or_else(|| env("GCLOUD_PROJECT")) {
        Some(project) => {
            vertex_adc = vertex_adc.extra("project", json!(project)).extra("location", json!(env("GOOGLE_CLOUD_LOCATION")));
        }
        None => vertex_adc.gate = Gate::Env(&["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"]),
    }
    let vertex_key = target("Google Vertex (API key)", Gate::Env(&["GOOGLE_CLOUD_API_KEY"]), "google-vertex", "gemini-3-flash-preview")
        .api_key(&env("GOOGLE_CLOUD_API_KEY").unwrap_or_default());
    run_matrix(
        "responseid.test.ts",
        vec![
            target("Google", Gate::Env(&["GEMINI_API_KEY"]), "google", "gemini-2.5-flash"),
            vertex_adc,
            vertex_key,
            target("OpenAI Completions", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-4o-mini").api_without_compat("openai-completions"),
            target("OpenAI Responses", Gate::Env(&["OPENAI_API_KEY"]), "openai", "gpt-5-mini"),
            target("Anthropic", Gate::Env(&["ANTHROPIC_API_KEY"]), "anthropic", "claude-sonnet-4-5"),
            target("Azure OpenAI Responses", Gate::Azure, "azure", "gpt-4o-mini"),
            target("Mistral", Gate::Env(&["MISTRAL_API_KEY"]), "mistral", "devstral-medium-latest"),
            target("GitHub Copilot OpenAI path", Gate::OAuth("github-copilot"), "github-copilot", "gpt-5.3-codex"),
            target("GitHub Copilot Anthropic path", Gate::OAuth("github-copilot"), "github-copilot", "claude-sonnet-4.6"),
            target("OpenAI Codex", Gate::OAuth("openai-codex"), "openai-codex", "gpt-6.1-sol"),
        ],
        &[("should expose responseId", expect_response_id)],
        3,
    )
    .await;
}

