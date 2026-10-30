//! Reusable harness for `AgentSession` tests (port of `test/suite/harness.ts` and the parts of
//! `test/utilities.ts` it uses).
//!
//! Use it from an integration test with:
//!
//! ```ignore
//! mod support;
//! use support::*;
//! ```
//!
//! The harness wires a real [`AgentSession`] to pi-ai's faux provider: no network, no API keys.
//! Responses are queued with [`Harness::set_responses`] / [`Harness::append_responses`].
#![allow(dead_code)]

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pi_agent::types::{AgentMessage, AgentToolRef};
use pi_ai::auth::types::{ApiKeyCredential, Credential};
use pi_ai::compat::{FauxProviderRegistration, register_faux_provider};
use pi_ai::providers::faux::{FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions};
use pi_ai::types::{Model, ToolResultMessage, UserContent};
use pi_coding_agent::core::agent_session::{
    AgentSession, AgentSessionConfig, AgentSessionConfigExtensionRunnerRef, AgentSessionEvent,
    AgentSessionEventListener,
};
use pi_coding_agent::core::auth_storage::AuthStorage;
use pi_coding_agent::core::event_bus::create_event_bus;
use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, load_extension_from_factory,
};
use pi_coding_agent::core::extensions::types::{
    AfterProviderResponseEvent, ExtensionEvent, ExtensionFactory, ExtensionRef,
    LoadExtensionsResult, ProviderChatModelConfig, ProviderConfig, ProviderModelConfig,
};
use pi_coding_agent::core::messages::convert_to_llm;
use pi_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pi_coding_agent::core::resource_loader::{
    ResourceExtensionPaths, ResourceLoader, ResourceLoaderGetAgentsFilesResult,
    ResourceLoaderGetAppendSystemPromptSourcesResult, ResourceLoaderGetPromptsResult,
    ResourceLoaderGetSkillsResult, ResourceLoaderGetSystemPromptSourceResult,
    ResourceLoaderGetThemesResult, ResourceLoaderReloadOptions,
};
use pi_coding_agent::core::session_manager::SessionManager;
use pi_coding_agent::core::settings_manager::SettingsManager;
use serde_json::{Map, Value};

#[allow(unused_imports)]
pub use pi_ai::providers::faux::{
    FauxMessageOptions, faux_assistant_message, faux_text, faux_text_message, faux_thinking,
    faux_tool_call,
};

// ---------------------------------------------------------------------------
// Global api-registry isolation
// ---------------------------------------------------------------------------

/// `AgentSession::reload()` calls `reset_api_providers()` (as TS does), which clears every faux
/// registration in the process-global api registry. Vitest isolates test files; Rust tests share
/// one process, so faux-backed tests hold a shared guard while they run and tests that reload take
/// the exclusive one.
static API_REGISTRY_LOCK: std::sync::LazyLock<Arc<tokio::sync::RwLock<()>>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::RwLock::new(())));

/// Shared guard for tests whose faux providers live in the global api registry.
pub async fn shared_api_registry() -> tokio::sync::OwnedRwLockReadGuard<()> {
    API_REGISTRY_LOCK.clone().read_owned().await
}

/// Exclusive guard for tests that reset the global api registry (`session.reload()`). Polls
/// instead of queueing, so waiting never blocks new shared guards (a test may hold several), and
/// gives up after a minute so a leaked harness cannot hang the suite.
pub async fn exclusive_api_registry() -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Ok(guard) = API_REGISTRY_LOCK.clone().try_write_owned() {
            return Some(guard);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Text of a message's content (text blocks joined by `\n`).
pub fn get_message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(m) => user_content_text(&m.content),
        AgentMessage::ToolResult(m) => m
            .content
            .iter()
            .filter_map(|b| b.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Assistant(m) => m
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::System(_) => String::new(),
        AgentMessage::Custom(m) => match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        },
    }
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(t) => t.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// An inline extension for [`HarnessOptions::extension_factories`].
#[derive(Clone)]
pub struct TestExtension {
    pub factory: ExtensionFactory,
    /// Extension path; defaults to `<inline:N>`.
    pub path: Option<String>,
}

impl TestExtension {
    pub fn new(factory: ExtensionFactory) -> Self {
        Self {
            factory,
            path: None,
        }
    }

    pub fn with_path(factory: ExtensionFactory, path: impl Into<String>) -> Self {
        Self {
            factory,
            path: Some(path.into()),
        }
    }
}

/// Load inline extensions into a [`LoadExtensionsResult`] (TS `createTestExtensionsResult`).
pub async fn create_test_extensions_result(
    inputs: &[TestExtension],
    cwd: &str,
) -> anyhow::Result<LoadExtensionsResult> {
    let runtime = create_extension_runtime();
    let event_bus = create_event_bus();
    let mut extensions: Vec<ExtensionRef> = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let path = input
            .path
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| format!("<inline:{}>", index + 1));
        extensions.push(
            load_extension_from_factory(
                &input.factory,
                cwd,
                event_bus.clone(),
                runtime.clone(),
                Some(&path),
            )
            .await?,
        );
    }
    Ok(LoadExtensionsResult {
        extensions,
        errors: Vec::new(),
        warnings: None,
        runtime,
    })
}

/// A resource loader with no resources except the given extensions (TS `createTestResourceLoader`).
pub struct TestResourceLoader {
    pub extensions_result: LoadExtensionsResult,
    pub system_prompt: Mutex<Option<String>>,
    pub append_system_prompt: Mutex<Vec<String>>,
}

impl TestResourceLoader {
    pub fn new(extensions_result: Option<LoadExtensionsResult>) -> Arc<Self> {
        Arc::new(Self {
            extensions_result: extensions_result.unwrap_or_else(|| LoadExtensionsResult {
                extensions: Vec::new(),
                errors: Vec::new(),
                warnings: None,
                runtime: create_extension_runtime(),
            }),
            system_prompt: Mutex::new(None),
            append_system_prompt: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl ResourceLoader for TestResourceLoader {
    fn get_extensions(&self) -> LoadExtensionsResult {
        self.extensions_result.clone()
    }
    fn get_skills(&self) -> ResourceLoaderGetSkillsResult {
        ResourceLoaderGetSkillsResult {
            skills: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
    fn get_prompts(&self) -> ResourceLoaderGetPromptsResult {
        ResourceLoaderGetPromptsResult::default()
    }
    fn get_themes(&self) -> ResourceLoaderGetThemesResult {
        ResourceLoaderGetThemesResult::default()
    }
    fn get_agents_files(&self) -> ResourceLoaderGetAgentsFilesResult {
        ResourceLoaderGetAgentsFilesResult {
            agents_files: Vec::new(),
        }
    }
    fn get_system_prompt(&self) -> Option<String> {
        self.system_prompt.lock().clone()
    }
    fn get_system_prompt_source(&self) -> Option<ResourceLoaderGetSystemPromptSourceResult> {
        None
    }
    fn get_append_system_prompt(&self) -> Vec<String> {
        self.append_system_prompt.lock().clone()
    }
    fn get_append_system_prompt_sources(
        &self,
    ) -> Vec<ResourceLoaderGetAppendSystemPromptSourcesResult> {
        Vec::new()
    }
    fn extend_resources(&self, _paths: &ResourceExtensionPaths) {}
    async fn reload(&self, _options: Option<&ResourceLoaderReloadOptions>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Options of [`create_harness`].
#[derive(Default)]
pub struct HarnessOptions {
    /// Faux provider id. Default: pi-ai's faux default.
    pub provider: Option<String>,
    /// Faux models. Default: one faux model.
    pub models: Vec<FauxModelDefinition>,
    /// Settings JSON (`Partial<Settings>`).
    pub settings: Option<Value>,
    /// Base tools (`baseToolsOverride`).
    pub tools: Option<Vec<AgentToolRef>>,
    pub initial_active_tool_names: Option<Vec<String>>,
    pub allowed_tool_names: Option<Vec<String>>,
    pub excluded_tool_names: Option<Vec<String>>,
    pub resource_loader: Option<Arc<dyn ResourceLoader>>,
    pub extension_factories: Vec<TestExtension>,
    /// Default: true.
    pub with_configured_auth: Option<bool>,
    /// Session to continue, for example to test a resume. Default: a new in-memory session.
    pub session_manager: Option<Arc<SessionManager>>,
    /// Do not take the shared api-registry guard: the test holds [`exclusive_api_registry`].
    pub exclusive_api_registry: bool,
    /// Stream faux responses at this rate (default: instantly).
    pub faux_tokens_per_second: Option<f64>,
}

/// A session wired to the faux provider.
pub struct Harness {
    pub session: Arc<AgentSession>,
    pub session_manager: Arc<SessionManager>,
    pub settings_manager: Arc<SettingsManager>,
    pub model_runtime: Arc<ModelRuntime>,
    pub auth_storage: Arc<AuthStorage>,
    pub faux: FauxProviderRegistration,
    pub models: Vec<Model>,
    pub events: Arc<Mutex<Vec<AgentSessionEvent>>>,
    pub temp_dir: std::path::PathBuf,
    /// Shared api-registry guard, released by [`Harness::cleanup`] or on drop.
    pub registry_guard: Mutex<Option<tokio::sync::OwnedRwLockReadGuard<()>>>,
}

impl Harness {
    pub fn get_model(&self) -> Model {
        self.faux.get_model()
    }

    pub fn get_model_by_id(&self, id: &str) -> Option<Model> {
        self.faux.get_model_by_id(id)
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.faux.set_responses(responses);
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.faux.append_responses(responses);
    }

    pub fn get_pending_response_count(&self) -> usize {
        self.faux.get_pending_response_count()
    }

    /// Snapshot of all session events so far.
    pub fn events(&self) -> Vec<AgentSessionEvent> {
        self.events.lock().clone()
    }

    /// The `type` discriminants of the events so far.
    pub fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .iter()
            .map(|e| {
                serde_json::to_value(e)
                    .ok()
                    .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Events of one `type`, as JSON.
    pub fn events_of_type(&self, event_type: &str) -> Vec<Value> {
        self.events
            .lock()
            .iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some(event_type))
            .collect()
    }

    pub fn user_texts(&self) -> Vec<String> {
        self.session
            .messages()
            .iter()
            .filter(|m| matches!(m, AgentMessage::User(_)))
            .map(get_message_text)
            .collect()
    }

    pub fn assistant_texts(&self) -> Vec<String> {
        self.session
            .messages()
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant(_)))
            .map(get_message_text)
            .collect()
    }

    /// The latest result of `tool_name` in the session transcript.
    pub fn tool_result(&self, tool_name: &str) -> ToolResultMessage {
        self.session
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                AgentMessage::ToolResult(r) if r.tool_name == tool_name => Some(r.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("No {tool_name} tool result"))
    }

    pub fn cleanup(&self) {
        self.session.dispose();
        self.faux.unregister();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
        self.registry_guard.lock().take();
    }
}

fn create_temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-suite-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The faux models as provider model configs for `registerProvider`.
pub fn provider_models(faux: &FauxProviderRegistration) -> Vec<ProviderModelConfig> {
    faux.models()
        .iter()
        .map(|m| {
            ProviderModelConfig::Chat(ProviderChatModelConfig {
                id: m.id.clone(),
                name: m.name.clone(),
                api: Some(m.api.clone()),
                base_url: Some(m.base_url.clone()),
                input: m.input.clone(),
                input_limits: m.input_limits.clone(),
                cost: m.cost.clone(),
                headers: None,
                reasoning: m.reasoning,
                thinking_level_map: None,
                prompt_cache: None,
                context_window: m.context_window,
                max_tokens: m.max_tokens,
                sampling_params: None,
                compat: None,
            })
        })
        .collect()
}

/// Build a session on the faux provider (TS `createHarness`).
pub async fn create_harness(options: HarnessOptions) -> anyhow::Result<Harness> {
    let registry_guard = if options.exclusive_api_registry {
        None
    } else {
        Some(shared_api_registry().await)
    };
    let temp_dir = create_temp_dir();
    let cwd = temp_dir.to_string_lossy().into_owned();
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        provider: options.provider.clone(),
        models: options.models,
        tokens_per_second: options.faux_tokens_per_second,
        ..Default::default()
    });
    faux.set_responses(Vec::new());
    let model = faux.get_model();
    let with_configured_auth = options.with_configured_auth.unwrap_or(true);
    let extension_runner_ref = AgentSessionConfigExtensionRunnerRef::new();

    let session_manager = match options.session_manager {
        Some(sm) => sm,
        None => SessionManager::in_memory(Some(&cwd), None, None)?,
    };
    let settings_json: Map<String, Value> = match options.settings {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let settings_manager = SettingsManager::in_memory_json(settings_json, None);

    let mut credentials: IndexMap<String, Credential> = IndexMap::new();
    if with_configured_auth {
        credentials.insert(
            model.provider.clone(),
            Credential::ApiKey(ApiKeyCredential {
                key: Some("faux-key".into()),
                env: None,
            }),
        );
    }
    let auth_storage = AuthStorage::in_memory(Some(&credentials));
    let model_runtime = ModelRuntime::create(Some(&CreateModelRuntimeOptions {
        credentials: Some(auth_storage.clone()),
        models_path: None,
        allow_model_network: Some(false),
        ..Default::default()
    }))
    .await?;
    if with_configured_auth {
        model_runtime.register_provider(
            &model.provider,
            &ProviderConfig {
                base_url: Some(model.base_url.clone()),
                api_key: Some("faux-key".into()),
                api: Some(faux.api().to_string()),
                models: Some(provider_models(&faux)),
                ..Default::default()
            },
        )?;
    }

    let payload_ref = extension_runner_ref.clone();
    let response_ref = extension_runner_ref.clone();
    let context_ref = extension_runner_ref.clone();
    let agent = Agent::new(AgentOptions {
        get_api_key: Some(Arc::new(move |_provider| {
            let key = with_configured_auth.then(|| "faux-key".to_string());
            Box::pin(async move { key })
        })),
        // TS harness: `streamFn: streamSimple`.
        stream_fn: Some(pi_agent::compat_stream_simple_fn()),
        initial_state: Some(AgentInitialState {
            model: Some(model.clone()),
            system_prompt: Some(String::new()),
            tools: Some(Vec::new()),
            ..Default::default()
        }),
        convert_to_llm: Some(Arc::new(|messages: Vec<AgentMessage>| {
            let converted = convert_to_llm(&messages);
            Box::pin(async move { converted })
        })),
        on_payload: Some(Arc::new(move |payload: Value, _model: Model| {
            let runner = payload_ref.current();
            Box::pin(async move {
                match runner {
                    Some(runner) if runner.has_handlers("before_provider_request") => {
                        runner.emit_before_provider_request(payload).await.ok()
                    }
                    _ => None,
                }
            })
        })),
        on_response: Some(Arc::new(move |response, _model: Model| {
            let runner = response_ref.current();
            Box::pin(async move {
                if let Some(runner) = runner
                    && runner.has_handlers("after_provider_response")
                {
                    let _ = runner
                        .emit(ExtensionEvent::AfterProviderResponse(
                            AfterProviderResponseEvent {
                                status: response.status,
                                headers: response.headers,
                            },
                        ))
                        .await;
                }
            })
        })),
        transform_context: Some(Arc::new(move |messages: Vec<AgentMessage>, _signal| {
            let runner = context_ref.current();
            Box::pin(async move {
                match runner {
                    Some(runner) => runner.emit_context(&messages).await.unwrap_or(messages),
                    None => messages,
                }
            })
        })),
        ..Default::default()
    });

    let resource_loader: Arc<dyn ResourceLoader> = match options.resource_loader {
        Some(loader) => loader,
        None => {
            let extensions_result = if options.extension_factories.is_empty() {
                None
            } else {
                Some(create_test_extensions_result(&options.extension_factories, &cwd).await?)
            };
            TestResourceLoader::new(extensions_result)
        }
    };

    let tool_map = options.tools.map(|tools| {
        tools
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect::<IndexMap<String, AgentToolRef>>()
    });
    let uses_default_tools = options.initial_active_tool_names.is_none();
    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager: session_manager.clone(),
        settings_manager: settings_manager.clone(),
        cwd: cwd.clone(),
        scoped_models: None,
        resource_loader,
        custom_tools: None,
        model_runtime: model_runtime.clone(),
        cache_warmer: None,
        initial_active_tool_names: options.initial_active_tool_names,
        uses_default_tools: Some(uses_default_tools),
        default_tool_modifiers: None,
        allowed_tool_names: options.allowed_tool_names,
        excluded_tool_names: options.excluded_tool_names,
        base_tools_override: tool_map,
        extension_runner_ref: Some(extension_runner_ref),
        session_start_event: None,
    });

    let events: Arc<Mutex<Vec<AgentSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let listener: AgentSessionEventListener = Arc::new(move |event| sink.lock().push(event));
    session.subscribe(&listener);

    let models = faux.models().to_vec();
    Ok(Harness {
        session,
        session_manager,
        settings_manager,
        model_runtime,
        auth_storage,
        faux,
        models,
        events,
        temp_dir,
        registry_guard: Mutex::new(registry_guard),
    })
}
