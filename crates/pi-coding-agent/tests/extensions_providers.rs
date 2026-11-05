//! Native pi-ai provider registration from JavaScript extensions: `pi.registerProvider(provider)`
//! with providers built by `createProvider()` / `fauxProvider()`, JavaScript auth methods (OAuth
//! login through the interaction, api-key resolution), Rust built-in providers and API
//! implementations used from JavaScript (`builtinProviders()`, `anthropicMessagesApi()`), and the
//! shared compat API registry.

mod extensions_support;
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use pi_ai::auth::types::{AuthEvent, AuthInteraction, AuthPrompt, AuthType, Credential};
use pi_ai::types::{Context, Message, StopReason, UserContent, UserMessage};
use pi_coding_agent::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
use serde_json::json;
use support::*;

/// Write an extension into a fresh directory and start a session that loads it.
async fn session_with_extension(source: &str) -> (Harness, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pi-ext-provider-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("extension.ts");
    std::fs::write(&path, source).unwrap();
    let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        cwd: dir.display().to_string(),
        agent_dir: dir.display().to_string(),
        additional_extension_paths: Some(vec![path.display().to_string()]),
        no_skills: Some(true),
        no_prompt_templates: Some(true),
        no_context_files: Some(true),
        ..Default::default()
    }));
    loader.reload(None).await.unwrap();
    assert!(loader.get_extensions().errors.is_empty(), "{:#?}", loader.get_extensions().errors);
    let harness = create_harness(HarnessOptions { resource_loader: Some(loader), ..Default::default() }).await.unwrap();
    // Binding extensions applies their provider registrations to the model runtime.
    harness.session.bind_extensions(&Default::default()).await.unwrap();

    (harness, dir)
}

fn user(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage { content: UserContent::Text(text.to_string()), timestamp: 0, ..Default::default() })],
        tools: None,
    }
}

fn text_of(message: &pi_ai::types::AssistantMessage) -> String {
    message.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn js_native_provider_streams_through_the_model_runtime() {
    let (harness, dir) = session_with_extension(
        r#"
        import { fauxAssistantMessage, fauxProvider } from "@earendil-works/pi-ai";
        export default function (pi) {
            const faux = fauxProvider({ provider: "js-native", models: [{ id: "native-1", name: "Native One" }] });
            faux.setResponses([
                (context, options) => fauxAssistantMessage(`echo: ${context.messages.at(-1).content} key=${options?.apiKey ?? "none"}`),
            ]);
            pi.registerProvider(faux.provider);
        }
        "#,
    )
    .await;
    let runtime = &harness.model_runtime;
    let model = runtime.get_model("js-native", "native-1").expect("JS provider model is registered");
    assert_eq!(model.name, "Native One");
    let message = runtime.complete_simple(model, user("hi there"), None).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text_of(&message), "echo: hi there key=none");
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

/// Answers login prompts and records notifications.
#[derive(Default)]
struct ScriptedInteraction {
    prompts: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
}

#[async_trait]
impl AuthInteraction for ScriptedInteraction {
    async fn prompt(&self, prompt: AuthPrompt) -> anyhow::Result<String> {
        self.prompts.lock().push(prompt.message.clone());
        Ok("the-code".to_string())
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = event {
            self.events.lock().push(url);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn js_native_provider_oauth_login_refresh_and_auth() {
    let (harness, dir) = session_with_extension(
        r#"
        import { createProvider, createAssistantMessageEventStream } from "@earendil-works/pi-ai";
        export default function (pi) {
            pi.registerProvider(createProvider({
                id: "js-oauth",
                name: "JS OAuth",
                models: [{ id: "m1", name: "M1", api: "js-oauth-api", provider: "js-oauth", baseUrl: "http://localhost:0",
                    reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
                    contextWindow: 1000, maxTokens: 100 }],
                auth: {
                    oauth: {
                        name: "JS OAuth login",
                        async login(interaction) {
                            interaction.notify({ type: "auth_url", url: "https://example.test/authorize" });
                            const code = await interaction.prompt({ type: "manual_code", message: "Paste the code:" });
                            return { type: "oauth", access: `access-${code}`, refresh: "r1", expires: Date.now() + 3600_000 };
                        },
                        async refresh(credential) {
                            return { ...credential, access: `${credential.access}-refreshed`, expires: Date.now() + 3600_000 };
                        },
                        async toAuth(credential) {
                            return { apiKey: credential.access, headers: { "x-js-auth": "yes" } };
                        },
                    },
                },
                api: {
                    stream: (model, context, options) => {
                        const stream = createAssistantMessageEventStream();
                        const message = { role: "assistant", content: [{ type: "text", text: `key ${options.apiKey} header ${options.headers?.["x-js-auth"]}` }],
                            api: model.api, provider: model.provider, model: model.id, stopReason: "stop", timestamp: Date.now(),
                            usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } } };
                        queueMicrotask(() => { stream.push({ type: "done", reason: "stop", message }); stream.end(); });
                        return stream;
                    },
                    streamSimple(model, context, options) { return this.stream(model, context, options); },
                },
            }));
        }
        "#,
    )
    .await;
    let runtime = harness.model_runtime.clone();
    assert!(!runtime.has_configured_auth("js-oauth"));
    let interaction = Arc::new(ScriptedInteraction::default());
    let credential = runtime
        .login("js-oauth", AuthType::OAuth, interaction.clone(), None, None)
        .await
        .expect("OAuth login through the JS auth method");
    let Credential::OAuth(oauth) = credential else { panic!("expected an OAuth credential") };
    assert_eq!(oauth.access, "access-the-code");
    assert_eq!(*interaction.prompts.lock(), vec!["Paste the code:".to_string()]);
    assert_eq!(*interaction.events.lock(), vec!["https://example.test/authorize".to_string()]);
    let model = runtime.get_model("js-oauth", "m1").unwrap();
    let message = runtime.complete_simple(model, user("hi"), None).await;
    assert_eq!(text_of(&message), "key access-the-code header yes", "{:?}", message.error_message);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_api_and_providers_from_javascript() {
    let (base_url, requests) = extensions_support::mock_anthropic_server("mocked anthropic reply").await;
    let source = r#"
        import { anthropicMessagesApi, createProvider, envApiKeyAuth, getApiProvider } from "@earendil-works/pi-ai";
        import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
        import { Type } from "typebox";
        export default function (pi) {
            // A provider whose chat API is pi-ai's (Rust) Anthropic Messages implementation.
            pi.registerProvider(createProvider({
                id: "proxy-anthropic",
                baseUrl: "BASE_URL",
                auth: { apiKey: envApiKeyAuth("Proxy key", ["PI_TEST_PROXY_KEY"]) },
                models: [{ id: "claude-proxy", name: "Claude via proxy", api: "anthropic-messages", provider: "proxy-anthropic",
                    baseUrl: "BASE_URL", reasoning: false, input: ["text"], cost: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0 },
                    contextWindow: 200000, maxTokens: 1000 }],
                api: anthropicMessagesApi(),
            }));
            pi.registerTool({
                name: "probe",
                label: "Probe",
                description: "Inspect pi-ai from JavaScript",
                parameters: Type.Object({}),
                async execute() {
                    const anthropic = builtinProviders().find((p) => p.id === "anthropic");
                    const models = anthropic.getModels();
                    const auth = await anthropic.auth.apiKey.resolve({
                        ctx: { env: async (name) => (name === "ANTHROPIC_API_KEY" ? "sk-from-js-ctx" : undefined), fileExists: async () => false },
                        signal: new AbortController().signal,
                    });
                    const builtin = getApiProvider("anthropic-messages");
                    return { content: [{ type: "text", text: JSON.stringify({
                        provider: anthropic.name, hasModels: models.length > 0, apiKey: auth?.auth?.apiKey, source: auth?.source,
                        registryApi: builtin?.api, hasStream: typeof builtin?.stream === "function",
                    }) }], details: {} };
                },
            });
        }
    "#
    .replace("BASE_URL", &base_url);
    // SAFETY: only this test reads PI_TEST_PROXY_KEY.
    unsafe { std::env::set_var("PI_TEST_PROXY_KEY", "proxy-secret") };
    let (harness, dir) = session_with_extension(&source).await;
    let runtime = harness.model_runtime.clone();
    let model = runtime.get_model("proxy-anthropic", "claude-proxy").unwrap();
    let message = runtime.complete_simple(model, user("hello proxy"), None).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text_of(&message), "mocked anthropic reply");
    let request = requests.lock()[0].clone();
    assert!(request.starts_with("POST /v1/messages"), "{request}");
    assert!(request.to_ascii_lowercase().contains("x-api-key: proxy-secret"), "{request}");
    assert!(request.contains("hello proxy"), "{request}");

    let runner = harness.session.extension_runner();
    let probe = runner.get_tool_definition("probe").unwrap();
    let result = (probe.execute)("call".into(), json!({}), None, None, None).await.unwrap();
    let text = serde_json::to_value(&result.content).unwrap()[0]["text"].as_str().unwrap().to_string();
    let probe: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(probe["provider"], "Anthropic");
    assert_eq!(probe["hasModels"], true);
    assert_eq!(probe["apiKey"], "sk-from-js-ctx");
    assert_eq!(probe["source"], "ANTHROPIC_API_KEY");
    assert_eq!(probe["registryApi"], "anthropic-messages");
    assert_eq!(probe["hasStream"], true);
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}
