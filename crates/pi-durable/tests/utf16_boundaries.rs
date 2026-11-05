//! Lone UTF16 units crossing generic SDK boundaries: schema validation, provider
//! conversion and telemetry carriers. Expected values come from the pinned
//! TypeScript sources run directly under Node.
use pi_durable::{
    harness::{
        declaration::ToolDeclaration,
        tool::{ExecutableTool, NativeTool, ToolCall, ToolExecutionResult},
    },
    *,
};
use std::sync::Arc;

/// Parse a JSON value written with `\uXXXX` escapes, as `JSON.stringify` emits.
fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).unwrap()
}
fn text(literal: &str) -> JsonString {
    json(literal).as_string().unwrap().clone()
}
fn tool(parameters: &str) -> NativeTool {
    NativeTool::new(
        ToolDeclaration::new(text(r#""t\ud800""#), "", json(parameters)),
        Arc::new(|_, _, _| Box::pin(async { Ok(ToolExecutionResult::default()) })),
    )
}

#[test]
fn tool_arguments_validate_lone_surrogates_like_source() {
    // [parameters, arguments, Ok(JSON.stringify(result)) | Err(JSON.stringify(message))]
    let cases: [(&str, &str, std::result::Result<&str, &str>); 6] = [
        (
            r#"{"type":"object","properties":{"text":{"type":"string","pattern":"^.$","maxLength":1}},"required":["text"]}"#,
            r#"{"text":"\ud800"}"#,
            Ok(r#"{"text":"\ud800"}"#),
        ),
        (
            r#"{"type":"object","properties":{"text":{"type":"string"},"n":{"type":"number"}}}"#,
            r#"{"text":"a\udc00b","n":"5","k\ud800":"v\udfff"}"#,
            Ok(r#"{"text":"a\udc00b","n":5,"k\ud800":"v\udfff"}"#),
        ),
        (
            r#"{"type":"object","properties":{"s":{"type":"string","enum":["\ud800"]}},"required":["s"]}"#,
            r#"{"s":"\ud800"}"#,
            Ok(r#"{"s":"\ud800"}"#),
        ),
        (
            r#"{"type":"object","properties":{"s":{"type":"string","enum":["\ud800"]}},"required":["s"]}"#,
            r#"{"s":"\udc00"}"#,
            Err(
                r#""Validation failed for tool \"t\ud800\":\n  - s: must be equal to one of the allowed values\n\nReceived arguments:\n{\n  \"s\": \"\\udc00\"\n}""#,
            ),
        ),
        (
            r#"{"type":"object","properties":{"n":{"type":"number"}},"additionalProperties":false}"#,
            r#"{"n":1,"x\ud800":1}"#,
            Err(
                r#""Validation failed for tool \"t\ud800\":\n  - x\ud800: schema is false\n  - root: must not have additional properties\n\nReceived arguments:\n{\n  \"n\": 1,\n  \"x\\ud800\": 1\n}""#,
            ),
        ),
        (
            r#"{"type":"object","properties":{"s":{"type":"string","maxLength":1}}}"#,
            r#"{"s":"\ud800\ud800"}"#,
            Err(
                r#""Validation failed for tool \"t\ud800\":\n  - s: must not have more than 1 characters\n\nReceived arguments:\n{\n  \"s\": \"\\ud800\\ud800\"\n}""#,
            ),
        ),
    ];
    for (parameters, arguments, expected) in cases {
        let tool = tool(parameters);
        let call = ToolCall::new("1", text(r#""t\ud800""#), json(arguments));
        let actual = tool
            .validate_arguments(&call, json(arguments))
            .map(|value| value.into_value().to_string())
            .map_err(|error| error.message_text());
        match expected {
            Ok(value) => assert_eq!(actual.unwrap(), value, "{parameters}"),
            Err(message) => assert_eq!(actual.unwrap_err(), text(message), "{parameters}"),
        }
    }
}

#[test]
fn structured_output_validates_lone_surrogates_like_source() {
    let mut tool = tool(r#"{"type":"object","properties":{}}"#);
    tool.structured_output_schema = Some(json(
        r#"{"type":"object","properties":{"s":{"type":"string","maxLength":1}},"additionalProperties":false}"#,
    ));
    let cases = [
        (r#"{"s":"\ud800"}"#, None),
        (
            r#"{"s":"\ud800x"}"#,
            Some(
                r#""Tool t\ud800 returned structuredOutput that does not match its schema: s: must not have more than 1 characters""#,
            ),
        ),
        (
            r#"{"k\udc00":1}"#,
            Some(
                r#""Tool t\ud800 returned structuredOutput that does not match its schema: k\udc00: schema is false""#,
            ),
        ),
    ];
    for (value, expected) in cases {
        let result = ToolExecutionResult {
            structured_output: Some(json(value).into()),
            ..Default::default()
        };
        assert_eq!(
            tool.structured_output_error(&result).unwrap(),
            expected.map(text),
            "{value}"
        );
    }
}

struct StaticKey;
#[async_trait::async_trait]
impl pi_ai::auth::ApiKeyAuth for StaticKey {
    fn name(&self) -> &str {
        "Static"
    }
    async fn resolve(
        &self,
        _input: pi_ai::auth::ApiKeyResolveInput,
    ) -> anyhow::Result<Option<pi_ai::auth::AuthResult>> {
        Ok(Some(pi_ai::auth::AuthResult {
            auth: pi_ai::auth::ModelAuth {
                api_key: Some("sk-ant-test".into()),
                ..Default::default()
            },
            ..Default::default()
        }))
    }
}
/// The real pi-ai request builder for each probed API behind a local base URL.
struct LocalProvider {
    id: String,
    auth: pi_ai::auth::ProviderAuth,
    model: pi_ai::Model,
}
impl LocalProvider {
    fn models(model: pi_ai::Model) -> pi_ai::models::Models {
        let models = pi_ai::models::Models::default();
        models.set_provider(Arc::new(Self {
            id: model.provider.clone(),
            auth: pi_ai::auth::ProviderAuth {
                api_key: Some(Arc::new(StaticKey)),
                oauth: None,
            },
            model,
        }));
        models
    }
}
#[async_trait::async_trait]
impl pi_ai::Provider for LocalProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.id
    }
    fn auth(&self) -> &pi_ai::auth::ProviderAuth {
        &self.auth
    }
    fn get_models(&self) -> Vec<pi_ai::Model> {
        vec![self.model.clone()]
    }
    fn stream(
        &self,
        model: &pi_ai::Model,
        context: &pi_ai::TranscriptContext,
        options: pi_ai::StreamOptions,
    ) -> pi_ai::AssistantMessageEventStream {
        self.stream_simple(model, context, options)
    }
    fn stream_simple(
        &self,
        model: &pi_ai::Model,
        context: &pi_ai::TranscriptContext,
        options: pi_ai::StreamOptions,
    ) -> pi_ai::AssistantMessageEventStream {
        use pi_ai::api::*;
        match model.api.as_str() {
            "anthropic-messages" => anthropic_messages::stream_simple(model, context, options),
            "openai-completions" => openai_completions::stream_simple(model, context, options),
            "openai-responses" => openai_responses::stream_simple(model, context, options),
            "google-generative-ai" => google_generative_ai::stream_simple(model, context, options),
            "mistral-conversations" => {
                mistral_conversations::stream_simple(model, context, options)
            }
            "pi-messages" => pi_messages::stream_simple(model, context, options),
            "bedrock-converse-stream" => {
                bedrock_converse_stream::stream_simple(model, context, options)
            }
            api => panic!("unprobed API {api}"),
        }
    }
}
/// The request body the SDK sends for durable `messages` through `sdk_collection`.
async fn request_body(model: pi_ai::Model, messages: &[JsonValue]) -> JsonValue {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = model;
    let base = format!("http://{}", listener.local_addr().unwrap());
    model.base_url = if model.api.starts_with("openai") {
        format!("{base}/v1")
    } else {
        base
    };
    let reference = ModelRef {
        provider: model.provider.as_str().into(),
        model_id: model.id.as_str().into(),
    };
    let collection = harness::models::sdk_collection(LocalProvider::models(model));
    let resolved = collection.get_model(&reference).unwrap().unwrap();
    let capture = tokio::spawn(capture_one(listener));
    let messages = messages.iter().cloned().map(Arc::new).collect();
    let mut stream = collection
        .stream_simple(
            resolved,
            messages,
            JsonObject::default(),
            Context::default(),
        )
        .await
        .unwrap();
    let _ = stream.result().await;
    json(&String::from_utf8(capture.await.unwrap()).unwrap())
}

/// Accept one HTTP request, answer 400 and return its body.
async fn capture_one(listener: tokio::net::TcpListener) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
        data.extend_from_slice(&buffer[..read]);
        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse().unwrap())
            .unwrap();
        if data.len() >= end + 4 + length {
            let body = data[end + 4..end + 4 + length].to_vec();
            let reply =
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"captured"}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            return body;
        }
    }
}

#[tokio::test]
async fn provider_text_strips_lone_surrogates_like_source_request_bodies() {
    let model = pi_ai::providers::all::get_builtin_model("anthropic", "claude-haiku-4-5").unwrap();
    // Messages as durable JSON with unpaired units in every sanitized text field.
    let messages = [
        r#"{"role":"user","content":"hi\ud83d","timestamp":1}"#,
        r#"{"role":"assistant","content":[{"type":"text","text":"ok\ude00"},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"a"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-haiku-4-5","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":2}"#,
        r#"{"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"out\ud800"}],"isError":false,"timestamp":3}"#,
        r#"{"role":"user","content":[{"type":"text","text":"x\udfffy"}],"timestamp":4}"#,
    ]
    .map(json);
    let body = request_body(model, &messages).await;
    // Upstream `streamSimple` against a local server, pinned source.
    assert_eq!(
        body["messages"].to_string(),
        r#"[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"call_1","name":"read","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"out","is_error":false}]},{"role":"user","content":[{"type":"text","text":"xy","cache_control":{"type":"ephemeral"}}]}]"#
    );
}

/// Text made of unpaired surrogates (and whitespace) passes upstream's length and
/// `trim()` checks before `sanitizeSurrogates` empties it. Expected bodies are
/// upstream `streamSimple` requests captured at the HTTP layer from a local server
/// for six APIs and nine scenarios, including system text and sections:
/// `fixtures/utf16-provider-bodies.json`.
#[tokio::test]
async fn sanitized_text_keeps_upstream_blank_checks_in_request_bodies() {
    let fixture = json(include_str!("fixtures/utf16-provider-bodies.json"));
    let scenarios = fixture["scenarios"].as_array().unwrap();
    let mut failures = Vec::new();
    for (provider, id) in PROBE_MODELS {
        let model = pi_ai::providers::all::get_builtin_model(provider, id).unwrap();
        let expected = &fixture["bodies"][model.api.as_str()];
        for (index, scenario) in scenarios.iter().enumerate() {
            let text = scenario
                .to_string()
                .replace("\"API\"", &format!("\"{}\"", model.api))
                .replace("\"PROVIDER\"", &format!("\"{}\"", model.provider))
                .replace("\"MODEL\"", &format!("\"{}\"", model.id));
            let messages = json(&text).as_array().unwrap().to_vec();
            let body = request_body(model.clone(), &messages).await;
            for field in expected["fields"].as_array().unwrap() {
                let field = field.as_string().unwrap().to_string_lossy();
                let actual = body[field.as_str()].to_string();
                let wanted = expected["values"][index][field.as_str()].to_string();
                if actual != wanted {
                    failures.push(format!(
                        "{} #{index} {field}\n  upstream {wanted}\n  native   {actual}",
                        model.api
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn provider_conversion_keeps_rejecting_strings_the_sdk_cannot_carry() {
    let tool_call = Arc::new(json(
        r#"{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"read","arguments":{"path":"\ud800"}}],"api":"anthropic-messages","provider":"anthropic","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":2}"#,
    ));
    assert!(harness::models::provider_messages(&[tool_call], "anthropic-messages").is_err());
    // `pi-messages` sends `JSON.stringify` escapes rather than stripping text.
    let text = Arc::new(json(
        r#"{"role":"user","content":"hi\ud83d","timestamp":1}"#,
    ));
    assert!(
        harness::models::provider_messages(std::slice::from_ref(&text), "pi-messages").is_err()
    );
    assert!(harness::models::provider_messages(&[text], "openai-completions").is_ok());
    // Upstream sends these as `\\udXXX` escapes; the SDK's UTF8 types reject them explicitly.
    for message in [
        r#"{"role":"toolResult","toolCallId":"c\ud800","toolName":"read","content":[],"isError":false,"timestamp":3}"#,
        r#"{"role":"toolResult","toolCallId":"c","toolName":"r\ud800","content":[],"isError":false,"timestamp":3}"#,
        r#"{"role":"assistant","content":[{"type":"text","text":"t","textSignature":"\udc00"}],"api":"a","provider":"p","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}"#,
        r#"{"role":"system","content":"s","toolsAdded":[{"name":"t","description":"d\ud800","parameters":{}}],"timestamp":1}"#,
    ] {
        let valid = Arc::new(json(
            &message.replace("\\ud800", "x").replace("\\udc00", "x"),
        ));
        assert!(harness::models::provider_messages(&[valid], "anthropic-messages").is_ok());
        let message = Arc::new(json(message));
        assert!(harness::models::provider_messages(&[message], "anthropic-messages").is_err());
    }
    // Codex sends system text as unsanitized `instructions`; other APIs strip it.
    let system = Arc::new(json(
        r#"{"role":"system","content":"s\ud800","timestamp":1}"#,
    ));
    assert!(
        harness::models::provider_messages(std::slice::from_ref(&system), "openai-codex-responses")
            .is_err()
    );
    assert!(harness::models::provider_messages(&[system], "openai-responses").is_ok());
    let options = JsonObject::try_from_value(json(r#"{"headers":{"x":"\ud800"}}"#)).unwrap();
    assert!(harness::models::provider_options(&options).is_err());
}

#[test]
fn sdk_model_lookup_misses_identities_with_lone_surrogates() {
    let collection = harness::models::sdk_collection(pi_ai::models::Models::default());
    for reference in [
        ModelRef {
            provider: text(r#""p\ud800""#),
            model_id: "m".into(),
        },
        ModelRef {
            provider: "p".into(),
            model_id: text(r#""m\udc00""#),
        },
    ] {
        assert!(collection.get_model(&reference).unwrap().is_none());
    }
}

#[tokio::test]
async fn persisted_trace_and_link_carriers_keep_lone_surrogates() {
    use indexmap::IndexMap;
    use pi_durable::harness::{catalog::*, runtime::TaskRuntime, scheduler::*, state::*};
    use pi_telemetry::{InMemoryTelemetryContext, SpanOptions, SpanOutcome, TelemetryContext};
    let telemetry = InMemoryTelemetryContext::new();
    let creator = telemetry.begin_span(SpanOptions::new("creator"));
    // An adapter may carry any JavaScript string; unrelated entries must not drop the parent.
    let mut carrier = JsonObject::try_from_value(
        pi_chord::json::to_value(&telemetry.inject(creator.as_ref()).unwrap()).unwrap(),
    )
    .unwrap();
    carrier.insert(text(r#""x\ud800""#), json(r#""\udc00""#));
    let task: Task<JsonValue, JsonValue, JsonValue> = define_task(TaskDefinition {
        name: "test.carrier".into(),
        version: 1,
        initial: Arc::new(|_| Ok(json(r#"{"phase":"run"}"#))),
        phases: IndexMap::from([(
            "run".into(),
            Arc::new(
                |_: RunningTask,
                 runtime: TaskRuntime,
                 context: Context|
                 -> pi_durable::storage::StorageFuture<()> {
                    Box::pin(async move {
                        runtime
                            .commit(
                                |_, _| async {
                                    Ok(Some(NextTaskState::Terminal {
                                        outcome: TaskOutcome::Completed {
                                            result: JsonValue::Null,
                                        },
                                    }))
                                },
                                &context,
                            )
                            .await
                    })
                },
            ) as PhaseHandler<_, _, _>,
        )]),
        abort: Arc::new(
            |_: RunningTask,
             _: TaskRuntime,
             _: Context|
             -> pi_durable::storage::StorageFuture<()> { Box::pin(async { Ok(()) }) },
        ),
        migrate: None,
        hooks: None,
    });
    let catalog = TaskCatalog::default();
    catalog
        .replace([InstalledTask {
            task: task.erased(),
            extension: Some("tests".into()),
        }])
        .unwrap();
    let scheduler = TaskScheduler::open(
        Arc::new(MemoryStorage::new()),
        catalog,
        SchedulerOptions {
            telemetry: telemetry.clone(),
            ..Default::default()
        },
        SessionOptions::default(),
        &Context::default(),
    )
    .await
    .unwrap();
    let id = scheduler
        .session()
        .commit(
            move |tx| async move {
                let root = tx.create_root_conversation().await?;
                let mut options = TaskOptions::new(TaskOwnership::Conversation);
                options.conversation_id = Some(root.id);
                options.trace = Some(TaskTrace::Carrier(carrier.clone()));
                options.links = Some(vec![carrier]);
                Ok(tx
                    .create_task(&task, JsonValue::Null, options)
                    .await?
                    .untyped())
            },
            &Context::default(),
        )
        .await
        .unwrap();
    scheduler.resume().unwrap();
    creator.end(SpanOutcome::Ok);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        scheduler
            .wait_for_task(id, &Context::default())
            .await
            .unwrap();
        while telemetry.get_spans().iter().any(|span| !span.settled) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let spans = telemetry.get_spans();
    let phase = spans
        .iter()
        .find(|span| span.name == "pi.durable.phase")
        .unwrap();
    assert_eq!(phase.parent_context, Some(spans[0].context.clone()));
    assert_eq!(
        phase
            .links
            .iter()
            .map(|link| link.context.clone())
            .collect::<Vec<_>>(),
        [spans[0].context.clone()]
    );
    scheduler
        .session()
        .close(&Context::default())
        .await
        .unwrap();
}

/// The builtin models the upstream request-body probes reuse.
const PROBE_MODELS: [(&str, &str); 6] = [
    ("anthropic", "claude-haiku-4-5"),
    ("groq", "llama-3.1-8b-instant"),
    ("openai", "gpt-4o-mini"),
    ("google", "gemini-2.5-flash"),
    ("mistral", "mistral-small-latest"),
    ("amazon-bedrock", "amazon.nova-micro-v1:0"),
];

#[test]
#[ignore = "prints builtin model JSON for the upstream request-body probe"]
fn dump_probe_models() {
    let models: Vec<_> = PROBE_MODELS
        .iter()
        .map(|(provider, id)| pi_ai::providers::all::get_builtin_model(provider, id).unwrap())
        .collect();
    println!("MODELS {}", serde_json::to_string(&models).unwrap());
}

/// Open boundary, pinned: the SDK's `JSON.parse` emulation turns response escapes of
/// unpaired surrogates into U+FFFD (pi-json's native contract), where upstream keeps
/// the unit. Exact parity needs UTF-16-capable pi-ai response types.
#[test]
fn response_json_helpers_replace_lone_surrogate_escapes() {
    use pi_ai::utils::json_parse::*;
    let replaced = serde_json::json!({ "t": "a\u{FFFD}" });
    let text = r#"{"t":"a\ud83d"}"#;
    assert!(serde_json::from_str::<serde_json::Value>(text).is_err());
    assert_eq!(pi_json::js_json_parse(text).unwrap(), replaced);
    assert_eq!(parse_json_with_repair(text).unwrap(), replaced);
    assert_eq!(parse_streaming_json(Some(text)), replaced);
    let parsed = parse_tool_call_arguments(Some(text));
    assert_eq!(
        (
            parsed.arguments_error,
            serde_json::Value::Object(parsed.arguments)
        ),
        (None, replaced)
    );
}

/// Open boundary, pinned end to end: an Anthropic text delta escaping an unpaired
/// surrogate reaches the durable assistant value as U+FFFD.
#[tokio::test]
async fn anthropic_response_escapes_reach_durable_values_as_replacement_characters() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model =
        pi_ai::providers::all::get_builtin_model("anthropic", "claude-haiku-4-5").unwrap();
    model.base_url = format!("http://{}", listener.local_addr().unwrap());
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_test","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a\ud83d"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body: String = events
        .iter()
        .map(|data| {
            let kind = json(data)["type"].as_string().unwrap().to_string_lossy();
            format!("event: {kind}\ndata: {data}\n\n")
        })
        .collect();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        while !data.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            data.extend_from_slice(&buffer[..read]);
        }
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // Keep the socket until the client has read the response.
        let _ = socket.read(&mut buffer).await;
    });
    let reference = ModelRef {
        provider: "anthropic".into(),
        model_id: model.id.as_str().into(),
    };
    let collection = harness::models::sdk_collection(LocalProvider::models(model));
    let resolved = collection.get_model(&reference).unwrap().unwrap();
    let messages = vec![Arc::new(json(
        r#"{"role":"user","content":"hi","timestamp":1}"#,
    ))];
    let mut stream = collection
        .stream_simple(
            resolved,
            messages,
            JsonObject::default(),
            Context::default(),
        )
        .await
        .unwrap();
    let message = stream.result().await.unwrap();
    server.abort();
    assert_eq!(
        message.text_blocks().unwrap(),
        [JsonString::from("a\u{FFFD}")]
    );
}
