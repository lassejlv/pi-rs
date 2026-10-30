//! Port of `packages/coding-agent/src/core/sdk.ts`.

use crate::config::get_agent_dir;
use crate::core::agent_session::{
    AgentSession, AgentSessionConfig, AgentSessionConfigExtensionRunnerRef,
    AgentSessionConfigScopedModels,
};
use crate::core::auth_guidance::format_no_models_available_message;
use crate::core::cache_warmer::{
    CacheWarmRequest, CacheWarmer, CacheWarmerModels, CacheWarmerSessionManager,
    CacheWarmingDecisionEvent,
};
use crate::core::client_identity::get_client_identity;
use crate::core::defaults::DEFAULT_THINKING_LEVEL;
use crate::core::extensions::types::{AfterProviderResponseEvent, ExtensionEvent, ProviderStreamEvent};
use crate::core::mcp_servers::create_tool_name_matcher;
use crate::core::messages::convert_to_llm;
use crate::core::model_resolver::{FindInitialModelOptions, find_initial_model};
use crate::core::model_runtime::CreateModelRuntimeOptions;
use crate::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions};
use crate::core::session_manager::{SessionEntry, UsageEntry, get_default_session_dir};
use crate::core::settings_manager::{
    DEFAULT_TOOL_NAMES, apply_tool_modifiers,
    get_tool_list_error, is_tool_modifier_str,
};
use crate::core::timings::time;
use crate::core::virtual_models::get_branch_selection;
use crate::utils::paths::resolve_path;
use pi_agent::agent::{Agent, AgentInitialState, AgentOptions, WeakAgent};
use pi_agent::types::{AgentMessage, ConvertToLlmFn, StreamFn};
use pi_ai::models::{ModelsRequestOptions, TransformHeadersFn, clamp_thinking_level};
use pi_ai::types::{
    Context, Message, OnPayload, OnProviderStreamEvent, OnResponse, ProviderHeaders,
    ProviderResponse, SimpleStreamOptions, TranscriptContext, UserContent, UserContentBlock,
};
use serde_json::Value;
use std::path::Path;
use crate::core::extensions::types::LoadExtensionsResult;
use crate::core::extensions::types::SessionStartEvent;
use crate::core::model_runtime::ModelRuntime;
use crate::core::resource_loader::ResourceLoader;
use crate::core::session_manager::SessionManager;
use crate::core::settings_manager::SettingsManager;
use pi_agent::types::ThinkingLevel;
use pi_ai::types::Model;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use crate::core::agent_session_runtime::*;
pub use crate::core::extensions::types::ExtensionAPI;
pub use crate::core::extensions::types::ExtensionCommandContext;
pub use crate::core::extensions::types::ExtensionContext;
pub use crate::core::extensions::types::ExtensionFactory;
pub use crate::core::extensions::types::InlineExtension;
pub use crate::core::extensions::types::ToolDefinition;
pub use crate::core::prompt_templates::PromptTemplate;
pub use crate::core::skills::Skill;
pub use crate::core::slash_commands::SlashCommandInfo;
pub use crate::core::slash_commands::SlashCommandSource;
pub use crate::core::tools::Tool;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgentSessionOptionsScopedModels {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CreateAgentSessionOptionsNoTools {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "builtin")]
    Builtin,
}

impl CreateAgentSessionOptionsNoTools {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Builtin => "builtin",
        }
    }
}

#[derive(Clone, Default)]
pub struct CreateAgentSessionOptions {
    /// Working directory for project-local discovery. Default: process.cwd()
    pub cwd: Option<String>,
    /// Global config directory. Default: ~/.pi/agent
    pub agent_dir: Option<String>,
    /// Canonical model/auth runtime. Defaults to a runtime using agentDir/auth.json and models.json.
    pub model_runtime: Option<Arc<ModelRuntime>>,
    /// Model to use. Default: from settings, else first available
    pub model: Option<Model>,
    /// Thinking level. Default: from settings, else 'medium' (clamped to model capabilities)
    pub thinking_level: Option<ThinkingLevel>,
    /// Models available for cycling (Ctrl+P in interactive mode)
    pub scoped_models: Option<Vec<CreateAgentSessionOptionsScopedModels>>,
    /// Optional default tool suppression mode when no explicit allowlist is provided.
    ///
    /// - "all": start with no tools enabled
    /// - "builtin": disable the default built-in tools (read, bash, edit, write)
    ///   but keep extension/custom tools enabled
    pub no_tools: Option<CreateAgentSessionOptionsNoTools>,
    /// Optional allowlist of tool names or patterns, where `*` matches any characters.
    ///
    /// When omitted, pi uses the resolved `defaultTools` setting for the initial
    /// selection when configured. Otherwise it enables the default built-in tools
    /// (read, bash, edit, write). Extension/custom tools remain enabled unless
    /// `noTools` changes that default. When provided, only matching tools are
    /// enabled. MCP tools stay registered for codemode and tool search unless an
    /// entry starts with `mcp__`; then only matching MCP tools are kept. An empty
    /// list, like `noTools: "all"`, disables MCP tools too.
    ///
    /// A list of only `+name` and `-name` entries is not an allowlist: it adds tools
    /// to or removes them from the default selection, like the `defaultTools` setting.
    /// These entries take exact names. Mixing them with plain entries throws.
    pub tools: Option<Vec<String>>,
    /// Optional denylist of tool names or patterns to disable. Applies after `tools` when both are
    /// provided, MCP tools included.
    pub exclude_tools: Option<Vec<String>>,
    /// Custom tools to register (in addition to built-in tools).
    pub custom_tools: Option<Vec<ToolDefinition>>,
    /// Resource loader. When omitted, DefaultResourceLoader is used.
    pub resource_loader: Option<Arc<dyn ResourceLoader>>,
    /// Session manager. Default: SessionManager.create(cwd)
    pub session_manager: Option<Arc<SessionManager>>,
    /// Settings manager. Default: SettingsManager.create(cwd, agentDir)
    pub settings_manager: Option<Arc<SettingsManager>>,
    /// Session start event metadata for extension runtime startup.
    pub session_start_event: Option<SessionStartEvent>,
}

/// Result from createAgentSession
#[derive(Clone)]
pub struct CreateAgentSessionResult {
    /// The created session
    pub session: Arc<AgentSession>,
    /// Extensions result (for UI context setup in interactive mode)
    pub extensions_result: LoadExtensionsResult,
    /// Warning if session was restored with a different model than saved
    pub model_fallback_message: Option<String>,
}

/// Create an AgentSession with the specified options.
///
/// @example
/// ```text
/// // Minimal - uses defaults
/// const { session } = await createAgentSession();
///
/// // With explicit model
/// import { getModel } from '@earendil-works/pi-ai';
/// const { session } = await createAgentSession({
///   model: getModel('anthropic', 'claude-opus-4-5'),
///   thinkingLevel: 'high',
/// });
///
/// // Continue previous session
/// const { session, modelFallbackMessage } = await createAgentSession({
///   continueSession: true,
/// });
///
/// // Full control
/// const loader = new DefaultResourceLoader({
///   cwd: process.cwd(),
///   agentDir: getAgentDir(),
///   settingsManager: SettingsManager.create(),
/// });
/// await loader.reload();
/// const { session } = await createAgentSession({
///   model: myModel,
///   tools: ["read", "bash"],
///   resourceLoader: loader,
///   sessionManager: SessionManager.inMemory(),
/// });
/// ```
pub async fn create_agent_session(
    options: Option<&CreateAgentSessionOptions>,
) -> anyhow::Result<CreateAgentSessionResult> {
    install_default_stream_fn();
    let options = options.cloned().unwrap_or_default();
    let cwd = resolve_path(
        &options
            .cwd
            .clone()
            .or_else(|| options.session_manager.as_ref().map(|sm| sm.get_cwd()))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".into())
            }),
        None,
        None,
    );
    let agent_dir = match &options.agent_dir {
        Some(dir) => resolve_path(dir, None, None),
        None => get_agent_dir(),
    };

    let model_runtime = match &options.model_runtime {
        Some(runtime) => runtime.clone(),
        None => {
            let (auth_path, models_path) = match &options.agent_dir {
                Some(_) => (
                    Some(Path::new(&agent_dir).join("auth.json").to_string_lossy().into_owned()),
                    Some(Path::new(&agent_dir).join("models.json").to_string_lossy().into_owned()),
                ),
                None => (None, None),
            };
            ModelRuntime::create(Some(&CreateModelRuntimeOptions {
                auth_path,
                models_path,
                ..Default::default()
            }))
            .await?
        }
    };

    let settings_manager = options
        .settings_manager
        .clone()
        .unwrap_or_else(|| SettingsManager::create(&cwd, Some(&agent_dir), None));
    let session_manager = match &options.session_manager {
        Some(sm) => sm.clone(),
        None => SessionManager::create(
            &cwd,
            Some(&get_default_session_dir(&cwd, Some(&agent_dir))?),
            None,
        )?,
    };

    let resource_loader: Arc<dyn ResourceLoader> = match &options.resource_loader {
        Some(loader) => loader.clone(),
        None => {
            let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
                cwd: cwd.clone(),
                agent_dir: agent_dir.clone(),
                settings_manager: Some(settings_manager.clone()),
                ..Default::default()
            }));
            loader.reload(None).await?;
            time("resourceLoader.reload", None);
            loader
        }
    };

    // Check if session has existing data to restore
    let existing_session = session_manager.build_session_context();
    let has_existing_session = !existing_session.messages.is_empty();
    let has_thinking_entry = session_manager
        .get_branch(None)
        .iter()
        .any(|e| matches!(e, SessionEntry::ThinkingLevelChange(_)));

    let mut model = options.model.clone();
    let mut model_fallback_message: Option<String> = None;

    // Assistant messages name the physical model that answered, so a virtual selection is only in
    // model_change entries.
    let runtime_for_lookup = model_runtime.clone();
    let session_model = get_branch_selection(
        &session_manager.get_branch(None),
        Arc::new(move |provider: String, model_id: String| {
            runtime_for_lookup.get_model(&provider, &model_id)
        }),
    );

    // If session has data, try to restore model from it
    if model.is_none()
        && has_existing_session
        && let Some(session_model) = &session_model
    {
        let restored = model_runtime.get_model(&session_model.provider, &session_model.model_id);
        if let Some(restored) = restored
            && model_runtime.has_configured_auth(&restored.provider)
        {
            model = Some(restored);
        }
        if model.is_none() {
            model_fallback_message = Some(format!(
                "Could not restore model {}/{}",
                session_model.provider, session_model.model_id
            ));
        }
    }

    // If still no model, use findInitialModel (checks settings default, then provider defaults)
    if model.is_none() {
        let result = find_initial_model(&FindInitialModelOptions {
            scoped_models: Vec::new(),
            is_continuing: has_existing_session,
            default_provider: settings_manager.get_default_provider(),
            default_model_id: settings_manager.get_default_model(),
            default_thinking_level: settings_manager.get_default_thinking_level(),
            model_thinking_levels: Some(settings_manager.get_all_model_thinking_levels()),
            model_runtime: model_runtime.clone(),
        })
        .await?;
        model = result.model;
        match &model {
            None => model_fallback_message = Some(format_no_models_available_message()),
            Some(model) => {
                if let Some(message) = &mut model_fallback_message {
                    message.push_str(&format!(". Using {}/{}", model.provider, model.id));
                }
            }
        }
    }

    let mut thinking_level = options.thinking_level;

    // If session has data, restore thinking level from it
    if thinking_level.is_none() && has_existing_session {
        thinking_level = Some(if has_thinking_entry {
            // TS casts the stored string unchecked; an unknown level clamps to the model's lowest
            // supported level below, which is what clamping "off" yields.
            serde_json::from_value(Value::String(existing_session.thinking_level.clone()))
                .unwrap_or(ThinkingLevel::Off)
        } else {
            settings_manager
                .get_default_thinking_level()
                .unwrap_or(DEFAULT_THINKING_LEVEL)
        });
    }

    // Fall back to per-model override, then global default
    if thinking_level.is_none()
        && let Some(model) = &model
        && let Some(per_model) = settings_manager.get_model_thinking_level_setting(&model.provider, &model.id)
        && per_model.is_truthy()
    {
        thinking_level = Some(per_model.effective_level());
    }
    let mut thinking_level = thinking_level.unwrap_or_else(|| {
        settings_manager
            .get_default_thinking_level()
            .unwrap_or(DEFAULT_THINKING_LEVEL)
    });

    // Clamp to model capabilities
    thinking_level = match &model {
        None => ThinkingLevel::Off,
        Some(model) => clamp_thinking_level(model, thinking_level),
    };

    if let Some(tools) = &options.tools
        && let Some(error) = get_tool_list_error(tools)
    {
        anyhow::bail!("Invalid tools option: {error}");
    }
    let default_tool_names: Vec<String> = if options.no_tools.is_some() {
        Vec::new()
    } else {
        settings_manager
            .get_default_tools()
            .unwrap_or_else(|| DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect())
    };
    // A `tools` list of only `+name`/`-name` entries changes the default selection instead of
    // replacing it, like the `defaultTools` setting.
    let tool_modifiers: Option<Vec<String>> = options
        .tools
        .as_ref()
        .filter(|tools| tools.iter().any(|t| is_tool_modifier_str(t)))
        .cloned();
    let selected_tool_names: Option<Vec<String>> = match &tool_modifiers {
        Some(modifiers) => Some(apply_tool_modifiers(&default_tool_names, modifiers)),
        None => options.tools.clone(),
    };
    let allowed_tool_names: Option<Vec<String>> = if tool_modifiers.is_some() {
        if options.no_tools == Some(CreateAgentSessionOptionsNoTools::All) {
            selected_tool_names.clone()
        } else {
            None
        }
    } else {
        options.tools.clone().or_else(|| {
            (options.no_tools == Some(CreateAgentSessionOptionsNoTools::All)).then(Vec::new)
        })
    };
    let excluded_tool_names = options.exclude_tools.clone();
    let is_excluded_tool = excluded_tool_names
        .as_ref()
        .map(|names| create_tool_name_matcher(names));
    let initial_active_tool_names: Vec<String> = selected_tool_names
        .clone()
        .unwrap_or_else(|| default_tool_names.clone())
        .into_iter()
        .filter(|name| !is_excluded_tool.as_ref().is_some_and(|m| m(name.clone())))
        .collect();

    // Create convertToLlm wrapper that filters images if blockImages is enabled (defense-in-depth)
    let block_settings = settings_manager.clone();
    let convert_to_llm_with_block_images: ConvertToLlmFn = Arc::new(move |messages: Vec<AgentMessage>| {
        let converted = convert_to_llm(&messages);
        // Check setting dynamically so mid-session changes take effect
        let result = if block_settings.get_block_images() {
            block_images(converted)
        } else {
            converted
        };
        Box::pin(async move { result })
    });

    let extension_runner_ref = AgentSessionConfigExtensionRunnerRef::new();
    let warmer_runtime = model_runtime.clone();
    let warmer_sm_append = session_manager.clone();
    let warmer_sm_branch = session_manager.clone();
    let warmer_settings = settings_manager.clone();
    let warmer_runner_ref = extension_runner_ref.clone();
    let cache_warmer = CacheWarmer::new(
        &CacheWarmerModels {
            stream_simple: Arc::new(move |model, context, options| {
                warmer_runtime.stream_simple(model, context, options)
            }),
        },
        &CacheWarmerSessionManager {
            append_usage: Arc::new(move |kind, provider, model, usage, note| {
                warmer_sm_append
                    .append_usage(&kind, &provider, &model, usage.clone(), note.as_deref())
                    .unwrap_or_else(|_| UsageEntry {
                        id: String::new(),
                        parent_id: None,
                        timestamp: String::new(),
                        kind,
                        provider,
                        model,
                        usage,
                        note,
                        extra: serde_json::Map::new(),
                    })
            }),
            get_branch: Arc::new(move |from| warmer_sm_branch.get_branch(from.as_deref())),
        },
        Arc::new(move || warmer_settings.get_cache_warming_mode()),
        Some(Arc::new(move |event: CacheWarmingDecisionEvent| {
            let runner = warmer_runner_ref.current();
            Box::pin(async move {
                match runner {
                    Some(runner) => runner.emit_cache_warming_decision(&event).await,
                    None => Ok(event.action),
                }
            })
        })),
    );

    let request_settings = settings_manager.clone();
    let request_runner_ref = extension_runner_ref.clone();
    let build_request_options = move |options: SimpleStreamOptions| -> ModelsRequestOptions {
        let provider_retry_settings = request_settings.get_provider_retry_settings();
        let http_idle_timeout_ms = request_settings.get_http_idle_timeout_ms().unwrap_or(0);
        let effective_timeout_ms: u64 = if http_idle_timeout_ms == 0 {
            2_147_483_647
        } else {
            http_idle_timeout_ms.max(0) as u64
        };
        let mut options = options;
        options.timeout_ms = options.timeout_ms.or_else(|| {
            Some(
                provider_retry_settings
                    .timeout_ms
                    .map(|t| t.max(0) as u64)
                    .unwrap_or(effective_timeout_ms),
            )
        });
        options.websocket_connect_timeout_ms = options.websocket_connect_timeout_ms.or_else(|| {
            request_settings
                .get_web_socket_connect_timeout_ms()
                .ok()
                .flatten()
                .map(|t| t.max(0) as u64)
        });
        options.max_retries = options
            .max_retries
            .or(provider_retry_settings.max_retries.map(|r| r.max(0) as u32));
        options.max_retry_delay_ms = options
            .max_retry_delay_ms
            .or(Some(provider_retry_settings.max_retry_delay_ms.max(0) as u64));
        // Replaces the agent loop's default identity; read per request so telemetry changes apply at once.
        options.identity = Some(get_client_identity(request_settings.clone()));
        let header_runner = request_runner_ref.current();
        let transform_headers: TransformHeadersFn = Arc::new(move |headers: ProviderHeaders| {
            let runner = header_runner.clone();
            Box::pin(async move {
                match runner {
                    Some(runner) if runner.has_handlers("before_provider_headers") => runner
                        .emit_before_provider_headers(headers.clone())
                        .await
                        .unwrap_or(headers),
                    _ => headers,
                }
            })
        });
        ModelsRequestOptions {
            options,
            transform_headers: Some(transform_headers),
        }
    };

    let payload_runner_ref = extension_runner_ref.clone();
    let transform_provider_payload: OnPayload = Arc::new(move |payload: Value, _model: Model| {
        let runner = payload_runner_ref.current();
        Box::pin(async move {
            match runner {
                Some(runner) if runner.has_handlers("before_provider_request") => {
                    runner.emit_before_provider_request(payload).await.ok()
                }
                _ => None,
            }
        })
    });
    let response_runner_ref = extension_runner_ref.clone();
    let handle_provider_response: OnResponse = Arc::new(move |response: ProviderResponse, _model: Model| {
        let runner = response_runner_ref.current();
        Box::pin(async move {
            if let Some(runner) = runner
                && runner.has_handlers("after_provider_response")
            {
                let _ = runner
                    .emit(ExtensionEvent::AfterProviderResponse(AfterProviderResponseEvent {
                        status: response.status,
                        headers: response.headers,
                    }))
                    .await;
            }
        })
    });
    let stream_event_runner_ref = extension_runner_ref.clone();
    let handle_provider_stream_event: OnProviderStreamEvent = Arc::new(move |data: Value, model: Model| {
        let runner = stream_event_runner_ref.current();
        // Handlers are awaited in stream order, so slow handlers delay stream consumption.
        Box::pin(async move {
            let Some(runner) = runner else {
                return;
            };
            if !runner.has_handlers("provider_stream_event") {
                return;
            }
            let _ = runner
                .emit(ExtensionEvent::ProviderStream(ProviderStreamEvent {
                    data,
                    provider: model.provider.clone(),
                    api: model.api.clone(),
                    model: model.id.clone(),
                }))
                .await;
        })
    });

    // Warm only requests for the selected model. Requests a virtual selection routed, or that an
    // extension redirected, may not be repeated by the next request, so warming them could be wasted.
    let agent_slot: Arc<parking_lot::RwLock<Option<WeakAgent>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let stream_agent_slot = agent_slot.clone();
    let stream_session_manager = session_manager.clone();
    let stream_runtime = model_runtime.clone();
    let stream_warmer = cache_warmer.clone();
    let stream_fn: StreamFn = Arc::new(move |model: Model, context: TranscriptContext, options: SimpleStreamOptions| {
        let session_id = options.session_id.clone();
        let request_options = build_request_options(options);
        let context = Context {
            system_prompt: None,
            messages: context.messages,
            tools: None,
        };
        // Compaction and summaries use their own routing ids; only session requests
        // replace the cache entry, so warming restarts from them.
        // Upgrade outside the `if let`: a guard in the scrutinee would stay held across the
        // warmer call below.
        let current_agent = stream_agent_slot.read().as_ref().and_then(|w| w.upgrade());
        if session_id.as_deref() == Some(stream_session_manager.get_session_id().as_str())
            && let Some(agent) = current_agent
        {
            let messages = agent.messages();
            let request_model = model.clone();
            let weak = agent.downgrade();
            let is_current: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                let Some(agent) = weak.upgrade() else {
                    return false;
                };
                let current_model = agent.model();
                let current_messages = agent.messages();
                current_model.provider == request_model.provider
                    && current_model.id == request_model.id
                    && messages.len() <= current_messages.len()
                    && messages.iter().zip(&current_messages).all(|(a, b)| a == b)
            });
            stream_warmer.start(
                &CacheWarmRequest {
                    model: model.clone(),
                    context: context.clone(),
                    options: request_options.clone(),
                },
                is_current,
            );
        }
        let stream = stream_runtime.stream_simple(model, context, Some(request_options));
        Box::pin(async move { Ok(stream) })
    });

    let context_runner_ref = extension_runner_ref.clone();
    let agent = Agent::new(AgentOptions {
        initial_state: Some(AgentInitialState {
            system_prompt: Some(String::new()),
            model: model.clone(),
            thinking_level: Some(thinking_level),
            tools: Some(Vec::new()),
            messages: Some(existing_session.messages.clone()),
        }),
        convert_to_llm: Some(convert_to_llm_with_block_images),
        transform_context: Some(Arc::new(move |messages: Vec<AgentMessage>, _signal| {
            let runner = context_runner_ref.current();
            Box::pin(async move {
                match runner {
                    Some(runner) => match runner.emit_context(&messages).await {
                        Ok(transformed) => transformed,
                        Err(_) => messages,
                    },
                    None => messages,
                }
            })
        })),
        stream_fn: Some(stream_fn),
        on_payload: Some(transform_provider_payload),
        on_response: Some(handle_provider_response),
        on_provider_stream_event: Some(handle_provider_stream_event),
        session_id: Some(session_manager.get_session_id()),
        steering_mode: Some(settings_manager.get_steering_mode()),
        follow_up_mode: Some(settings_manager.get_follow_up_mode()),
        transport: serde_json::to_value(settings_manager.get_transport())
            .ok()
            .and_then(|v| serde_json::from_value(v).ok()),
        thinking_budgets: settings_manager
            .get_thinking_budgets()
            .and_then(|b| serde_json::to_value(b).ok())
            .and_then(|v| serde_json::from_value(v).ok()),
        max_retry_delay_ms: Some(
            settings_manager
                .get_provider_retry_settings()
                .max_retry_delay_ms
                .max(0) as u64,
        ),
        ..Default::default()
    });
    *agent_slot.write() = Some(agent.downgrade());

    // Restore missing settings metadata for older sessions.
    if has_existing_session {
        if !has_thinking_entry {
            session_manager.append_thinking_level_change(thinking_level.as_str())?;
        }
    } else {
        // Save initial model and thinking level for new sessions so they can be restored on resume
        if let Some(model) = &model {
            session_manager.append_model_change(&model.provider, &model.id)?;
        }
        session_manager.append_thinking_level_change(thinking_level.as_str())?;
    }

    let uses_default_tools =
        (options.tools.is_none() || tool_modifiers.is_some()) && options.no_tools.is_none();
    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager,
        settings_manager,
        cwd,
        scoped_models: options.scoped_models.clone().map(|models| {
            models
                .into_iter()
                .map(|m| AgentSessionConfigScopedModels {
                    model: m.model,
                    thinking_level: m.thinking_level,
                })
                .collect()
        }),
        resource_loader: resource_loader.clone(),
        custom_tools: options.custom_tools.clone(),
        model_runtime,
        cache_warmer: Some(cache_warmer),
        initial_active_tool_names: Some(initial_active_tool_names),
        uses_default_tools: Some(uses_default_tools),
        default_tool_modifiers: tool_modifiers,
        allowed_tool_names,
        excluded_tool_names,
        base_tools_override: None,
        extension_runner_ref: Some(extension_runner_ref),
        session_start_event: options.session_start_event.clone(),
    });

    let extensions_result = resource_loader.get_extensions();
    Ok(CreateAgentSessionResult {
        session,
        extensions_result,
        model_fallback_message,
    })
}


const IMAGE_DISABLED_TEXT: &str = "Image reading is disabled.";

/// Replace images in user and tool-result messages with a text placeholder, deduplicating
/// consecutive placeholders.
fn block_images(messages: Vec<Message>) -> Vec<Message> {
    fn filter(content: Vec<UserContentBlock>) -> Option<Vec<UserContentBlock>> {
        if !content.iter().any(|c| matches!(c, UserContentBlock::Image(_))) {
            return None;
        }
        let mapped: Vec<UserContentBlock> = content
            .into_iter()
            .map(|c| match c {
                UserContentBlock::Image(_) => UserContentBlock::text(IMAGE_DISABLED_TEXT),
                other => other,
            })
            .collect();
        let mut result: Vec<UserContentBlock> = Vec::with_capacity(mapped.len());
        for (i, block) in mapped.iter().enumerate() {
            let is_placeholder =
                |b: &UserContentBlock| b.as_text().is_some_and(|t| t.text == IMAGE_DISABLED_TEXT);
            if is_placeholder(block) && i > 0 && is_placeholder(&mapped[i - 1]) {
                continue;
            }
            result.push(block.clone());
        }
        Some(result)
    }
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                if let UserContent::Blocks(blocks) = &user.content
                    && let Some(filtered) = filter(blocks.clone())
                {
                    user.content = UserContent::Blocks(filtered);
                }
                Message::User(user)
            }
            Message::ToolResult(mut result) => {
                if let Some(filtered) = filter(result.content.clone()) {
                    result.content = filtered;
                }
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

/// Preserve the pre-0.81 fallback for extensions that construct Agent instances or invoke
/// low-level agent loops without supplying a stream function (TS: `setDefaultStreamFn(streamSimple)`
/// when the SDK module loads).
fn install_default_stream_fn() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        if pi_agent::get_default_stream_fn().is_err() {
            pi_agent::set_default_stream_fn(Some(Arc::new(
                |model: Model, context: TranscriptContext, options: SimpleStreamOptions| {
                    let context = Context {
                        system_prompt: None,
                        messages: context.messages,
                        tools: None,
                    };
                    let stream = pi_ai::compat::stream_simple(&model, &context, options);
                    Box::pin(async move { Ok(stream) })
                },
            )));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{ImageContent, ToolResultMessage, UserMessage};

    fn image() -> UserContentBlock {
        UserContentBlock::Image(ImageContent { data: "AAAA".into(), mime_type: "image/png".into() })
    }

    #[test]
    fn block_images_replaces_and_dedupes_placeholders() {
        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    UserContentBlock::text("look"),
                    image(),
                    image(),
                ]),
                timestamp: 0,
                ..Default::default()
            }),
            Message::ToolResult(ToolResultMessage {
                content: vec![image()],
                ..Default::default()
            }),
        ];
        let result = block_images(messages);
        let Message::User(user) = &result[0] else {
            panic!()
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!()
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].as_text().unwrap().text, IMAGE_DISABLED_TEXT);
        let Message::ToolResult(tool) = &result[1] else {
            panic!()
        };
        assert_eq!(tool.content[0].as_text().unwrap().text, IMAGE_DISABLED_TEXT);
    }
}
