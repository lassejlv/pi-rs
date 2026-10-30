//! `sampling-options.test.ts`, `transform-messages-copilot-openai-to-anthropic.test.ts`,
//! `transform-messages-cross-model-thinking.test.ts`, `system-message-replay.test.ts`,
//! `telemetry-options.test.ts` and `fetch-option.test.ts`.

use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_ai::api::transform_messages::transform_messages;
use pi_ai::utils::text::{get_system_message_text, render_system_message_update};
use pi_ai::utils::transcript::*;
use pi_ai::*;
use serde_json::{Value, json};

use crate::mock_http::{MockServer, Response};
use crate::support::*;

// ---------------------------------------------------------------------------
// Port of sampling-options.test.ts
// ---------------------------------------------------------------------------

fn sampling_model(api: &str, sampling: Option<Value>, overrides: Value) -> Model {
    let mut value = json!({"id": "custom-model", "name": "Custom Model", "api": api, "provider": "custom-provider", "baseUrl": "http://127.0.0.1:9/v1",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 16384});
    if let Some(sampling) = sampling {
        value["samplingParams"] = sampling;
    }
    for (k, v) in overrides.as_object().unwrap() {
        value[k] = v.clone();
    }
    model(value)
}

fn params(value: Value) -> Option<SamplingParams> {
    Some(value.as_object().cloned().unwrap())
}

async fn direct(model: &Model, options: StreamOptions) -> Value {
    stream_payload(model, &user_ctx("Hello"), options).await
}

async fn simple(model: &Model, options: StreamOptions) -> Value {
    simple_payload(model, &user_ctx("Hello"), options).await
}

#[tokio::test]
async fn sampling_params_merge_into_openai_compatible_requests() {
    let completions = sampling_model("openai-completions", None, json!({}));
    let p = direct(&completions, StreamOptions { sampling_params: params(json!({"top_p": 0.95, "top_k": 0, "min_p": 0})), ..Default::default() }).await;
    assert_eq!((p["top_p"].clone(), p["top_k"].clone(), p["min_p"].clone()), (json!(0.95), json!(0), json!(0)));
    let p = direct(&completions, StreamOptions::default()).await;
    assert!(p.get("temperature").is_none() && p.get("top_p").is_none());

    // Model defaults apply to direct stream() calls too (#9506); request keys win.
    for api in ["openai-completions", "openai-responses", "azure-openai-responses"] {
        let model = sampling_model(api, Some(json!({"top_p": 0.95, "min_p": 0.05})), json!({}));
        let p = direct(&model, StreamOptions { sampling_params: params(json!({"top_p": 0.5})), ..Default::default() }).await;
        assert_eq!((p["top_p"].clone(), p["min_p"].clone()), (json!(0.5), json!(0.05)), "{api}");
    }

    let p = simple(&completions, StreamOptions { sampling_params: params(json!({"top_p": 0.5})), ..Default::default() }).await;
    assert_eq!(p["top_p"], 0.5);

    let leveled = sampling_model("openai-completions", Some(json!({"temperature": 1, "top_p": 0.95})), json!({
        "reasoning": true, "thinkingLevelMap": {"low": null, "medium": null}, "samplingParamsByThinkingLevel": {"high": {"temperature": 0.8, "top_k": 64}}
    }));
    let p = simple(&leveled, StreamOptions { reasoning: Some(ThinkingLevel::Low), ..Default::default() }).await;
    assert_eq!((p["temperature"].clone(), p["top_p"].clone(), p["top_k"].clone()), (json!(0.8), json!(0.95), json!(64)));

    let off = sampling_model("openai-completions", None, json!({"samplingParamsByThinkingLevel": {"off": {"temperature": 0.7}}}));
    assert_eq!(simple(&off, StreamOptions::default()).await["temperature"], 0.7);

    let low = sampling_model("openai-completions", None, json!({"reasoning": true, "samplingParamsByThinkingLevel": {"low": {"temperature": 0.6, "top_p": 0.95}}}));
    let p = simple(&low, StreamOptions { reasoning: Some(ThinkingLevel::Low), sampling_params: params(json!({"top_p": 0.5})), ..Default::default() }).await;
    assert_eq!((p["temperature"].clone(), p["top_p"].clone()), (json!(0.6), json!(0.5)));

    for api in ["openai-completions", "openai-responses", "azure-openai-responses"] {
        let model = sampling_model(api, Some(json!({"temperature": 1, "top_p": 0.95})), json!({"reasoning": true, "samplingParamsByThinkingLevel": {"low": {"temperature": 0.6, "top_k": 64}}}));
        let options = StreamOptions { sampling_params: params(json!({"top_p": 0.5})), ..Default::default() }.with_extra("reasoningEffort", "low");
        let p = direct(&model, options).await;
        assert_eq!((p["temperature"].clone(), p["top_p"].clone(), p["top_k"].clone()), (json!(0.6), json!(0.5), json!(64)), "{api}");
    }

    for api in ["openai-responses", "azure-openai-responses"] {
        let model = sampling_model(api, None, json!({"reasoning": true, "samplingParamsByThinkingLevel": {"off": {"temperature": 0.7}, "medium": {"temperature": 0.8}}}));
        let p = direct(&model, StreamOptions::default().with_extra("reasoningSummary", "auto")).await;
        assert_eq!(p["reasoning"]["effort"], "medium", "{api}");
        assert_eq!(p["temperature"], 0.8, "{api}");
    }

    let p = direct(&completions, StreamOptions { temperature: Some(0.0), sampling_params: params(json!({"temperature": 1})), ..Default::default() }).await;
    assert_eq!(p["temperature"], 1);

    let anthropic = sampling_model("anthropic-messages", None, json!({}));
    let p = direct(&anthropic, StreamOptions { sampling_params: params(json!({"top_p": 0.9, "top_k": 40})), ..Default::default() }).await;
    assert!(p.get("top_p").is_none() && p.get("top_k").is_none());
}

// ---------------------------------------------------------------------------
// Port of transform-messages-copilot-openai-to-anthropic.test.ts
// ---------------------------------------------------------------------------

fn copilot_claude() -> Model {
    model(json!({"id": "claude-sonnet-4.6", "name": "Claude Sonnet 4.6", "api": "anthropic-messages", "provider": "github-copilot",
        "baseUrl": "https://api.individual.githubcopilot.com", "reasoning": true, "input": ["text", "image"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 128000, "maxTokens": 16000}))
}

fn anthropic_normalize(id: &str, _model: &Model, _source: &AssistantMessage) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect()
}

fn gpt5_assistant(content: Value) -> Value {
    json!({"role": "assistant", "content": content, "api": "openai-responses", "provider": "github-copilot", "model": "gpt-5", "stopReason": "toolUse"})
}

fn messages(value: Value) -> Vec<Message> {
    ctx(json!({"messages": value})).messages
}

#[test]
fn copilot_openai_history_migrates_to_anthropic() {
    let model = copilot_claude();
    let history = messages(json!([
        {"role": "user", "content": "run a command"},
        gpt5_assistant(json!([{"type": "toolCall", "id": "call_123", "name": "bash", "arguments": {"command": "ls"},
            "thoughtSignature": json!({"type": "reasoning.encrypted", "id": "call_123", "data": "encrypted"}).to_string()}])),
        {"role": "toolResult", "toolCallId": "call_123", "toolName": "bash", "content": [{"type": "text", "text": "output"}], "isError": false}
    ]));
    let result = transform_messages(&history, &model, Some(&anthropic_normalize));
    let call = result.iter().find_map(Message::as_assistant).unwrap().tool_calls().next().unwrap().clone();
    assert_eq!(call.thought_signature, None);

    let history = messages(json!([
        {"role": "user", "content": "read the file"},
        gpt5_assistant(json!([{"type": "toolCall", "id": "call_123|fc_123", "name": "read", "arguments": {"path": "README.md"}}]))
    ]));
    let result = transform_messages(&history, &model, Some(&anthropic_normalize));
    let last = serde_json::to_value(result.last().unwrap()).unwrap();
    assert_eq!(last["role"], "toolResult");
    assert_eq!(last["toolCallId"], "call_123_fc_123");
    assert_eq!(last["toolName"], "read");
    assert_eq!(last["isError"], true);
    assert_eq!(last["content"], json!([{"type": "text", "text": "No result provided"}]));

    let history = messages(json!([
        {"role": "user", "content": "run commands"},
        gpt5_assistant(json!([
            {"type": "toolCall", "id": "call_1|fc_1", "name": "read", "arguments": {"path": "README.md"}},
            {"type": "toolCall", "id": "call_2|fc_2", "name": "bash", "arguments": {"command": "pwd"}}
        ])),
        {"role": "toolResult", "toolCallId": "call_1|fc_1", "toolName": "read", "content": [{"type": "text", "text": "done"}], "isError": false}
    ]));
    let result = transform_messages(&history, &model, Some(&anthropic_normalize));
    let synthetic: Vec<Value> = result
        .iter()
        .filter_map(Message::as_tool_result)
        .filter(|r| r.is_error)
        .map(|r| serde_json::to_value(Message::ToolResult(r.clone())).unwrap())
        .collect();
    assert_eq!(synthetic.len(), 1);
    assert_eq!(synthetic[0]["toolCallId"], "call_2_fc_2");
    assert_eq!(synthetic[0]["toolName"], "bash");
    assert_eq!(synthetic[0]["content"], json!([{"type": "text", "text": "No result provided"}]));
}

// ---------------------------------------------------------------------------
// Port of transform-messages-cross-model-thinking.test.ts
// ---------------------------------------------------------------------------

fn kimi() -> Model {
    model(json!({"id": "moonshotai/Kimi-K3", "name": "Kimi K3", "api": "openai-completions", "provider": "sference", "baseUrl": "https://api.sference.com/v1",
        "reasoning": true, "input": ["text", "image"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1048576, "maxTokens": 32768}))
}

fn sference(model_id: &str, content: Value, stop: &str) -> Value {
    json!({"role": "assistant", "content": content, "api": "openai-completions", "provider": "sference", "model": model_id, "stopReason": stop})
}

fn thinking(text: &str) -> Value {
    json!({"type": "thinking", "thinking": text, "thinkingSignature": "reasoning_content"})
}

#[test]
fn foreign_model_thinking_is_dropped() {
    // https://github.com/earendil-works/pi/issues/10995
    let history = messages(json!([
        {"role": "user", "content": "Why does curl work but node fails?"},
        sference("zai-org/GLM-5.3", json!([
            thinking("The user asks about ECONNRESET. Candidates: proxy, IPv6, TLS."),
            {"type": "text", "text": "Let me check the TLS versions."},
            {"type": "toolCall", "id": "call_1", "name": "bash", "arguments": {"command": "curl -v"}}
        ]), "toolUse"),
        {"role": "toolResult", "toolCallId": "call_1", "toolName": "bash", "content": [{"type": "text", "text": "TLSv1.2"}], "isError": false}
    ]));
    let result = transform_messages(&history, &kimi(), None);
    let assistant = result.iter().find_map(Message::as_assistant).unwrap();
    assert_eq!(
        serde_json::to_value(&assistant.content).unwrap(),
        json!([{"type": "text", "text": "Let me check the TLS versions."}, {"type": "toolCall", "id": "call_1", "name": "bash", "arguments": {"command": "curl -v"}}])
    );

    let history = messages(json!([
        {"role": "user", "content": "1"}, sference("moonshotai/Kimi-K3", json!([thinking("kimi 1"), {"type": "text", "text": "a"}]), "stop"),
        {"role": "user", "content": "2"}, sference("zai-org/GLM-5.3", json!([thinking("glm 1"), {"type": "text", "text": "b"}]), "stop"),
        {"role": "user", "content": "3"}, sference("moonshotai/Kimi-K3", json!([thinking("kimi 2"), {"type": "text", "text": "c"}]), "stop"),
        {"role": "user", "content": "4"}, sference("zai-org/GLM-5.3", json!([thinking("glm 2"), {"type": "text", "text": "d"}]), "stop"),
        {"role": "user", "content": "hi"}
    ]));
    let result = transform_messages(&history, &kimi(), None);
    let contents: Vec<Value> = result.iter().filter_map(Message::as_assistant).map(|a| serde_json::to_value(&a.content).unwrap()).collect();
    assert_eq!(contents, vec![
        json!([thinking("kimi 1"), {"type": "text", "text": "a"}]),
        json!([{"type": "text", "text": "b"}]),
        json!([thinking("kimi 2"), {"type": "text", "text": "c"}]),
        json!([{"type": "text", "text": "d"}]),
    ]);
}

// ---------------------------------------------------------------------------
// Port of system-message-replay.test.ts
// ---------------------------------------------------------------------------

fn tool_value(name: &str, description: Option<&str>) -> Value {
    json!({"name": name, "description": description.map(str::to_string).unwrap_or(format!("{name} tool")), "parameters": {"type": "object", "properties": {}}})
}

fn tool_of(name: &str, description: Option<&str>) -> Tool {
    serde_json::from_value(tool_value(name, description)).unwrap()
}

fn replay_transcript() -> TranscriptContext {
    normalize_context(&ctx(json!({"messages": [
        {"role": "system", "content": "base", "sections": {"a": "<a>1</a>", "b": "<b>1</b>"}, "toolsAdded": [tool_value("first", None)], "timestamp": 10},
        {"role": "user", "content": "hello", "timestamp": 11},
        {"role": "system", "content": "also do this", "timestamp": 12},
        {"role": "assistant", "content": [{"type": "text", "text": "ok"}], "timestamp": 13},
        {"role": "system", "content": "", "sections": {"a": "<a>2</a>", "b": null, "c": "<c>1</c>"}, "toolsRemoved": [{"name": "first"}], "toolsAdded": [tool_value("second", None)], "timestamp": 14}
    ]})))
}

#[test]
fn system_message_replay() {
    let transcript = replay_transcript();
    let current = get_current_system_message(&transcript.messages).unwrap();
    assert_eq!(
        serde_json::to_value(Message::System(current)).unwrap(),
        json!({"role": "system", "content": "base\n\nalso do this", "sections": {"a": "<a>2</a>", "c": "<c>1</c>"}, "toolsAdded": [tool_value("second", None)], "timestamp": 10})
    );
    assert_eq!(get_current_system_prompt(&transcript.messages), "base\n\nalso do this\n\n<a>2</a>\n\n<c>1</c>");

    let collapsed = collapse_system_messages(&transcript);
    assert_eq!(collapsed.messages.iter().map(|m| m.role()).collect::<Vec<_>>(), ["system", "user", "assistant"]);
    assert_eq!(collapse_system_messages(&collapsed), collapsed);

    let plain = normalize_context(&user_ctx("hi"));
    assert!(get_current_system_message(&plain.messages).is_none());
    assert_eq!(get_current_system_prompt(&plain.messages), "");
    assert_eq!(collapse_system_messages(&plain).messages, plain.messages);

    let late = normalize_context(&ctx(json!({"messages": [
        {"role": "user", "content": "old session", "timestamp": 1},
        {"role": "system", "content": "", "sections": {"preamble": "You are pi."}, "toolsAdded": [tool_value("x", None)], "timestamp": 2}
    ]})));
    assert_eq!(get_current_system_prompt(&late.messages), "You are pi.");
    let head = serde_json::to_value(&collapse_system_messages(&late).messages[0]).unwrap();
    assert_eq!(head["role"], "system");
    assert_eq!(head["toolsAdded"], json!([tool_value("x", None)]));

    let (Some(leading), Some(update)) = (transcript.messages[0].as_system(), transcript.messages[4].as_system()) else { panic!("expected system messages") };
    assert_eq!(get_system_message_text(leading), "base\n\n<a>1</a>\n\n<b>1</b>");
    assert_eq!(
        render_system_message_update(update),
        ["Updated system prompt section \"a\":\n\n<a>2</a>", "Removed system prompt section \"b\".", "Updated system prompt section \"c\":\n\n<c>1</c>"].join("\n\n")
    );
}

#[test]
#[allow(deprecated)]
fn legacy_prompt_fields_and_tool_state_helpers() {
    let base = vec![Message::User(UserMessage::new("hi", 1))];
    assert_eq!(normalize_context(&Context { messages: base.clone(), ..Default::default() }).messages, base);
    assert_eq!(normalize_context(&Context { system_prompt: Some(String::new()), tools: Some(vec![]), messages: base.clone() }).messages, base);
    let with_prompt = normalize_context(&Context { system_prompt: Some("be brief".into()), tools: Some(vec![tool_of("a", None)]), messages: base.clone() });
    assert_eq!(
        serde_json::to_value(&with_prompt.messages).unwrap(),
        json!([{"role": "system", "content": "be brief", "toolsAdded": [tool_value("a", None)], "timestamp": 0}, {"role": "user", "content": "hi", "timestamp": 1}])
    );

    assert!(declarations_equal(&tool_of("a", None), &tool_of("a", None)));
    assert!(!declarations_equal(&tool_of("a", None), &tool_of("a", Some("changed"))));
    let mut disabled = tool_of("a", None);
    disabled.constrained_sampling = Some(ConstrainedSampling::Disabled);
    assert!(!declarations_equal(&tool_of("a", None), &disabled));

    let changes = get_tool_state_changes(&[tool_of("a", None), tool_of("b", None)], &[tool_of("b", Some("changed")), tool_of("c", None)]);
    assert_eq!(changes.tools_added, vec![tool_of("b", Some("changed")), tool_of("c", None)]);
    assert_eq!(changes.tools_removed.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    let unchanged = get_tool_state_changes(&[tool_of("a", None)], &[tool_of("a", None)]);
    assert!(unchanged.tools_added.is_empty() && unchanged.tools_removed.is_empty());

    let transcript = replay_transcript();
    assert!(has_non_additive_tool_changes(&transcript.messages));
    assert!(!has_tool_redefinitions(&transcript.messages));
    let additive = normalize_context(&ctx(json!({"messages": [
        {"role": "system", "content": "", "toolsAdded": [tool_value("a", None)], "timestamp": 1},
        {"role": "system", "content": "", "toolsAdded": [tool_value("b", None)], "timestamp": 2}
    ]})));
    assert!(!has_non_additive_tool_changes(&additive.messages));
    let redeclared = normalize_context(&ctx(json!({"messages": [
        {"role": "system", "content": "", "toolsAdded": [tool_value("a", None)], "timestamp": 1},
        {"role": "system", "content": "", "toolsAdded": [tool_value("a", Some("changed"))], "timestamp": 2}
    ]})));
    assert!(has_non_additive_tool_changes(&redeclared.messages));
    assert!(has_tool_redefinitions(&redeclared.messages));
}

// ---------------------------------------------------------------------------
// Port of telemetry-options.test.ts
// ---------------------------------------------------------------------------

type Observed = Arc<Mutex<Vec<Option<pi_telemetry::ContextRef>>>>;

struct TelemetryStreams(Observed);

impl TelemetryStreams {
    fn done(model: &Model) -> AssistantMessageEventStream {
        let (sender, stream) = AssistantMessageEventStream::new();
        let message = AssistantMessage::empty_for(model, 0);
        sender.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message });
        stream
    }
}

impl ProviderStreams for TelemetryStreams {
    fn stream(&self, model: &Model, _context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.lock().push(options.telemetry);
        Self::done(model)
    }
    fn stream_simple(&self, model: &Model, _context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.lock().push(options.telemetry);
        Self::done(model)
    }
    fn supports_fetch_deferred(&self) -> bool {
        true
    }
    fn fetch_deferred(&self, model: &Model, _handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.lock().push(options.telemetry);
        Self::done(model)
    }
    fn supports_cancel_deferred(&self) -> bool {
        true
    }
    fn cancel_deferred(&self, _model: &Model, _handle: &DeferredHandle, options: StreamOptions) -> BoxFuture<'static, anyhow::Result<()>> {
        self.0.lock().push(options.telemetry);
        Box::pin(async { Ok(()) })
    }
}

/// TS `{ name: "Test", resolve: async () => ({ auth: {} }) }`.
struct EmptyAuth;

#[async_trait::async_trait]
impl pi_ai::auth::ApiKeyAuth for EmptyAuth {
    fn name(&self) -> &str {
        "Test"
    }
    async fn resolve(&self, _input: pi_ai::auth::ApiKeyResolveInput) -> anyhow::Result<Option<pi_ai::auth::AuthResult>> {
        Ok(Some(pi_ai::auth::AuthResult { auth: Default::default(), env: None, source: None }))
    }
}

struct TelemetryImages(Observed);

#[async_trait::async_trait]
impl ProviderImages for TelemetryImages {
    async fn generate_images(&self, model: &ImageModel, _context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        self.0.lock().push(options.telemetry);
        AssistantImages { api: model.api.clone(), provider: model.provider.clone(), model: model.id.clone(), ..Default::default() }
    }
}

fn same(observed: &Observed, span: &pi_telemetry::SpanRef, count: usize) {
    let observed = observed.lock();
    assert_eq!(observed.len(), count);
    assert!(observed.iter().all(|s| s.as_ref().is_some_and(|s| std::ptr::addr_eq(Arc::as_ptr(s), Arc::as_ptr(span)))));
}

#[tokio::test]
async fn telemetry_span_survives_every_dispatch_path() {
    let span = pi_telemetry::noop_telemetry_context();
    let model = model(json!({"id": "model", "name": "Model", "api": "telemetry-test", "provider": "telemetry-provider", "baseUrl": "https://example.test",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100}));
    let context = normalize_context(&Context::default());
    let base = pi_ai::api::simple_options::build_base_options(&model, &context, &StreamOptions { telemetry: Some(span.clone()), ..Default::default() }, None);
    assert!(std::ptr::addr_eq(Arc::as_ptr(base.telemetry.as_ref().unwrap()), Arc::as_ptr(&span)));

    let observed: Observed = Arc::default();
    let provider = create_provider(CreateProviderOptions {
        id: model.provider.clone(),
        auth: pi_ai::auth::ProviderAuth { api_key: Some(Arc::new(EmptyAuth)), oauth: None },
        models: vec![AnyModel::Chat(model.clone())],
        api: Some(ProviderApi::Single(Arc::new(TelemetryStreams(observed.clone())))),
        ..Default::default()
    })
    .unwrap();
    let handle = DeferredHandle { provider: model.provider.clone(), model_id: model.id.clone(), api: model.api.clone(), id: "response".into(), expires_at: None, poll_after_ms: None, data: None };
    let with_span = || StreamOptions { telemetry: Some(span.clone()), ..Default::default() };
    provider.stream(&model, &context, with_span()).result().await;
    provider.stream_simple(&model, &context, with_span()).result().await;
    provider.fetch_deferred(&model, &handle, with_span()).result().await;
    provider.cancel_deferred(&model, &handle, with_span()).await.unwrap();
    let models = create_models(CreateModelsOptions { auth_context: Some(MapAuthContext::new(&[])), ..Default::default() });
    models.set_provider(provider);
    let ctx = Context::default();
    models.stream(&model, &ctx, with_span()).result().await;
    models.stream_simple(&model, &ctx, with_span()).result().await;
    models.fetch_deferred(&model, &handle, with_span()).await;
    let _ = models.cancel_deferred(&model, &handle, with_span()).await;
    same(&observed, &span, 8);

    let images_observed: Observed = Arc::default();
    let image_model: ImageModel = serde_json::from_value(json!({"type": "image", "id": "image-model", "name": "Image Model", "api": "telemetry-test-images",
        "provider": "telemetry-image-provider", "baseUrl": "https://example.test", "input": ["text"], "output": ["image"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}})).unwrap();
    pi_ai::images_api_registry::register_images_api_provider(
        pi_ai::images_api_registry::ImagesApiProvider { api: image_model.api.clone(), implementation: Arc::new(TelemetryImages(images_observed.clone())) },
        Some("ts-ported-telemetry"),
    );
    let images_context = ImagesContext { input: vec![UserContentBlock::text("circle")] };
    pi_ai::images::generate_images(&image_model, &images_context, with_span()).await.unwrap();
    pi_ai::images_api_registry::unregister_images_api_providers("ts-ported-telemetry");
    let mut images = indexmap::IndexMap::new();
    images.insert(image_model.api.clone(), Arc::new(TelemetryImages(images_observed.clone())) as Arc<dyn ProviderImages>);
    let models = create_models(CreateModelsOptions { auth_context: Some(MapAuthContext::new(&[])), ..Default::default() });
    models.set_provider(
        create_provider(CreateProviderOptions {
            id: image_model.provider.clone(),
            auth: pi_ai::auth::ProviderAuth { api_key: Some(Arc::new(EmptyAuth)), oauth: None },
            models: vec![AnyModel::Image(image_model.clone())],
            images,
            ..Default::default()
        })
        .unwrap(),
    );
    models.generate_images(&image_model, &images_context, with_span()).await;
    same(&images_observed, &span, 2);
}

// ---------------------------------------------------------------------------
// Port of fetch-option.test.ts
// ---------------------------------------------------------------------------

fn upstream_model(api: &str, port: u16) -> Model {
    model(json!({"id": "test-model", "name": "Test Model", "api": api, "provider": "test-provider", "baseUrl": format!("http://upstream.test:{port}/v1"),
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 10000, "maxTokens": 1000}))
}

/// A client that resolves the unresolvable `upstream.test` host to the mock server: requests
/// only reach the server through this custom client (the TS custom `fetch`).
fn custom_client(port: u16) -> reqwest::Client {
    reqwest::Client::builder().no_proxy().resolve("upstream.test", std::net::SocketAddr::from(([127, 0, 0, 1], port))).build().unwrap()
}

#[tokio::test]
async fn custom_http_clients_carry_every_http_adapter() {
    let server = MockServer::start(|_, _| Response::json(401, json!({"error": {"message": "upstream rejected request"}}))).await;
    let port: u16 = server.url.rsplit(':').next().unwrap().parse().unwrap();
    let options = |key: String| StreamOptions { api_key: Some(key), http_client: Some(custom_client(port)), max_retries: Some(0), ..Default::default() };
    let context = normalize_context(&user_ctx("hello"));
    let codex_key = {
        use base64::Engine;
        format!("header.{}.signature", base64::engine::general_purpose::STANDARD.encode(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "account"}}).to_string()))
    };
    let runs = [
        pi_ai::api::anthropic_messages::stream_simple(&upstream_model("anthropic-messages", port), &context, options("test-key".into())).result().await,
        pi_ai::api::openai_completions::stream_simple(&upstream_model("openai-completions", port), &context, options("test-key".into())).result().await,
        pi_ai::api::openai_responses::stream_simple(&upstream_model("openai-responses", port), &context, options("test-key".into())).result().await,
        pi_ai::api::azure_openai_responses::stream_simple(&upstream_model("azure-openai-responses", port), &context, options("test-key".into())).result().await,
        pi_ai::api::mistral_conversations::stream_simple(&upstream_model("mistral-conversations", port), &context, options("test-key".into())).result().await,
        pi_ai::api::openai_codex_responses::stream_simple(
            &upstream_model("openai-codex-responses", port),
            &context,
            StreamOptions { transport: Some(Transport::Sse), ..options(codex_key) },
        )
        .result()
        .await,
        pi_ai::api::pi_messages::stream_simple(&upstream_model("pi-messages", port), &context, options("test-key".into())).result().await,
    ];
    for result in &runs {
        assert_eq!(result.stop_reason, StopReason::Error, "{}", result.api);
    }
    assert_eq!(server.requests().len(), runs.len(), "{:?}", runs.iter().map(|r| r.error_message.clone()).collect::<Vec<_>>());

    let mut image_model: ImageModel = serde_json::from_value(json!({"type": "image", "id": "test-model", "name": "Test Model", "api": "openrouter-images",
        "provider": "openrouter", "baseUrl": format!("http://upstream.test:{port}/v1"), "input": ["text"], "output": ["image"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}})).unwrap();
    image_model.output = vec!["image".into()];
    let images = pi_ai::api::openrouter_images::generate_images(&image_model, &ImagesContext { input: vec![UserContentBlock::text("draw")] }, options("test-key".into())).await;
    assert_eq!(images.stop_reason, ImagesStopReason::Error);
    assert_eq!(server.requests().len(), runs.len() + 1);

    // Google adapters reject custom clients instead of silently bypassing them.
    let google = pi_ai::api::google_generative_ai::stream_simple(&upstream_model("google-generative-ai", port), &context, options("test-key".into())).result().await;
    let vertex = pi_ai::api::google_vertex::stream_simple(&upstream_model("google-vertex", port), &context, options("test-key".into())).result().await;
    assert!(google.error_message.as_deref().unwrap().contains("Custom fetch is not supported by the Google Generative AI adapter"));
    assert!(vertex.error_message.as_deref().unwrap().contains("Custom fetch is not supported by the Google Vertex adapter"));
    assert_eq!(server.requests().len(), runs.len() + 1);
}
