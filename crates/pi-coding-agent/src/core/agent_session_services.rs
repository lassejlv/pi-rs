//! Port of `packages/coding-agent/src/core/agent-session-services.ts`.

use crate::config::get_agent_dir;
use crate::core::extensions::runner::provider_config_input;
use crate::core::extensions::types::{FlagType, FlagValue};
use crate::core::extensions::types::SessionStartEvent;
use crate::core::extensions::types::ToolDefinition;
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use crate::core::resource_loader::ResourceLoaderReloadOptions;
use crate::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
use crate::core::sdk::{
    CreateAgentSessionOptions, CreateAgentSessionOptionsNoTools,
    CreateAgentSessionOptionsScopedModels, CreateAgentSessionResult, create_agent_session,
};
use crate::core::session_manager::SessionManager;
use crate::core::settings_manager::SettingsManager;
use crate::utils::paths::resolve_path;
use indexmap::IndexMap;
use pi_ai::models::ModelsRefreshOptions;
use std::path::Path;
use pi_agent::types::ThinkingLevel;
use pi_ai::types::Model;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentSessionRuntimeDiagnosticType {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
}

impl AgentSessionRuntimeDiagnosticType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Non-fatal issues collected while creating services or sessions.
///
/// Runtime creation returns diagnostics to the caller instead of printing or
/// exiting. The app layer decides whether warnings should be shown and whether
/// errors should abort startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRuntimeDiagnostic {
    pub r#type: AgentSessionRuntimeDiagnosticType,
    pub message: String,
}

/// Value of an extension flag given on the command line (`boolean | string`).
pub type CreateAgentSessionServicesOptionsExtensionFlagValues = FlagValue;

/// `Omit<DefaultResourceLoaderOptions, "cwd" | "agentDir" | "settingsManager">`: those three
/// fields are replaced by the service's own values.
pub type CreateAgentSessionServicesOptionsResourceLoaderOptions = DefaultResourceLoaderOptions;

/// Inputs for creating cwd-bound runtime services.
///
/// These services are recreated whenever the effective session cwd changes.
/// CLI-provided resource paths should be resolved to absolute paths before they
/// reach this function, so later cwd switches do not reinterpret them.
#[derive(Clone)]
pub struct CreateAgentSessionServicesOptions {
    pub cwd: String,
    pub agent_dir: Option<String>,
    pub settings_manager: Option<Arc<SettingsManager>>,
    pub model_runtime: Option<Arc<ModelRuntime>>,
    pub model_runtime_signal: Option<CancellationToken>,
    pub extension_flag_values:
        Option<IndexMap<String, CreateAgentSessionServicesOptionsExtensionFlagValues>>,
    pub resource_loader_options: Option<CreateAgentSessionServicesOptionsResourceLoaderOptions>,
    pub resource_loader_reload_options: Option<ResourceLoaderReloadOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgentSessionFromServicesOptionsScopedModels {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CreateAgentSessionFromServicesOptionsNoTools {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "builtin")]
    Builtin,
}

impl CreateAgentSessionFromServicesOptionsNoTools {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Builtin => "builtin",
        }
    }
}

/// Inputs for creating an AgentSession from already-created services.
///
/// Use this after services exist and any cwd-bound model/tool/session options
/// have been resolved against those services.
#[derive(Clone)]
pub struct CreateAgentSessionFromServicesOptions {
    pub services: AgentSessionServices,
    pub session_manager: Arc<SessionManager>,
    pub session_start_event: Option<SessionStartEvent>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub scoped_models: Option<Vec<CreateAgentSessionFromServicesOptionsScopedModels>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: Option<CreateAgentSessionFromServicesOptionsNoTools>,
    pub custom_tools: Option<Vec<ToolDefinition>>,
}

/// Coherent cwd-bound runtime services for one effective session cwd.
///
/// This is infrastructure only. The AgentSession itself is created separately so
/// session options can be resolved against these services first.
#[derive(Clone)]
pub struct AgentSessionServices {
    pub cwd: String,
    pub agent_dir: String,
    pub model_runtime: Arc<ModelRuntime>,
    pub settings_manager: Arc<SettingsManager>,
    pub resource_loader: Arc<dyn ResourceLoader>,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

pub(crate) fn apply_extension_flag_values(
    resource_loader: &dyn ResourceLoader,
    extension_flag_values: Option<&IndexMap<String, FlagValue>>,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    let Some(extension_flag_values) = extension_flag_values else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    let extensions_result = resource_loader.get_extensions();
    let mut registered_flags: IndexMap<String, FlagType> = IndexMap::new();
    for extension in &extensions_result.extensions {
        for (name, flag) in &extension.read().flags {
            registered_flags.insert(name.clone(), flag.flag_type);
        }
    }

    let mut unknown_flags: Vec<String> = Vec::new();
    for (name, value) in extension_flag_values {
        let Some(flag_type) = registered_flags.get(name) else {
            unknown_flags.push(name.clone());
            continue;
        };
        match (flag_type, value) {
            (FlagType::Boolean, _) => {
                extensions_result
                    .runtime
                    .flag_values
                    .lock()
                    .insert(name.clone(), FlagValue::Bool(true));
            }
            (FlagType::String, FlagValue::String(value)) => {
                extensions_result
                    .runtime
                    .flag_values
                    .lock()
                    .insert(name.clone(), FlagValue::String(value.clone()));
            }
            (FlagType::String, FlagValue::Bool(_)) => diagnostics.push(AgentSessionRuntimeDiagnostic {
                r#type: AgentSessionRuntimeDiagnosticType::Error,
                message: format!("Extension flag \"--{name}\" requires a value"),
            }),
        }
    }

    if !unknown_flags.is_empty() {
        diagnostics.push(AgentSessionRuntimeDiagnostic {
            r#type: AgentSessionRuntimeDiagnosticType::Error,
            message: format!(
                "Unknown option{}: {}",
                if unknown_flags.len() == 1 { "" } else { "s" },
                unknown_flags
                    .iter()
                    .map(|name| format!("--{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    diagnostics
}

fn registration_error(extension_path: &str, error: &anyhow::Error) -> AgentSessionRuntimeDiagnostic {
    AgentSessionRuntimeDiagnostic {
        r#type: AgentSessionRuntimeDiagnosticType::Error,
        message: format!("Extension \"{extension_path}\" error: {error}"),
    }
}

/// Register the providers and virtual models extensions queued while loading (TS
/// `createAgentSessionServices`), returning one diagnostic per failed registration.
pub(crate) fn apply_pending_extension_registrations(
    resource_loader: &dyn ResourceLoader,
    model_runtime: &ModelRuntime,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    let mut diagnostics = Vec::new();
    let extensions_result = resource_loader.get_extensions();
    let runtime = &extensions_result.runtime;
    let pending = std::mem::take(&mut *runtime.pending_provider_registrations.lock());
    for registration in pending {
        if let Err(error) = model_runtime.register_provider(
            &registration.name,
            &provider_config_input(&registration.config),
        ) {
            diagnostics.push(registration_error(&registration.extension_path, &error));
        }
    }
    let pending = std::mem::take(&mut *runtime.pending_native_provider_registrations.lock());
    for registration in pending {
        if let Err(error) = model_runtime.register_native_provider(registration.provider) {
            diagnostics.push(registration_error(&registration.extension_path, &error));
        }
    }
    let pending = std::mem::take(&mut *runtime.pending_virtual_model_registrations.lock());
    for registration in pending {
        if let Err(error) = model_runtime.register_virtual_model(&registration.definition) {
            diagnostics.push(registration_error(&registration.extension_path, &error));
        }
    }
    diagnostics
}

/// Create cwd-bound runtime services.
///
/// Returns services plus diagnostics. It does not create an AgentSession.
pub async fn create_agent_session_services(
    options: &CreateAgentSessionServicesOptions,
) -> anyhow::Result<AgentSessionServices> {
    let cwd = resolve_path(&options.cwd, None, None);
    let agent_dir = match &options.agent_dir {
        Some(dir) => resolve_path(dir, None, None),
        None => get_agent_dir(),
    };
    let model_runtime = match &options.model_runtime {
        Some(runtime) => runtime.clone(),
        None => {
            ModelRuntime::create(Some(&CreateModelRuntimeOptions {
                auth_path: Some(Path::new(&agent_dir).join("auth.json").to_string_lossy().into_owned()),
                models_path: Some(
                    Path::new(&agent_dir).join("models.json").to_string_lossy().into_owned(),
                ),
                signal: options.model_runtime_signal.clone(),
                ..Default::default()
            }))
            .await?
        }
    };
    let settings_manager = options
        .settings_manager
        .clone()
        .unwrap_or_else(|| SettingsManager::create(&cwd, Some(&agent_dir), None));
    let mut loader_options = options.resource_loader_options.clone().unwrap_or_default();
    loader_options.cwd = cwd.clone();
    loader_options.agent_dir = agent_dir.clone();
    loader_options.settings_manager = Some(settings_manager.clone());
    let resource_loader = Arc::new(DefaultResourceLoader::new(&loader_options));
    resource_loader
        .reload(options.resource_loader_reload_options.as_ref())
        .await?;

    let mut diagnostics = apply_pending_extension_registrations(resource_loader.as_ref(), &model_runtime);
    model_runtime
        .refresh(Some(ModelsRefreshOptions {
            allow_network: Some(false),
            ..Default::default()
        }))
        .await?;
    diagnostics.extend(apply_extension_flag_values(
        resource_loader.as_ref(),
        options.extension_flag_values.as_ref(),
    ));

    Ok(AgentSessionServices {
        cwd,
        agent_dir,
        model_runtime,
        settings_manager,
        resource_loader,
        diagnostics,
    })
}

/// Create an AgentSession from previously created services.
///
/// This keeps session creation separate from service creation so callers can
/// resolve model, thinking, tools, and other session inputs against the target
/// cwd before constructing the session.
pub async fn create_agent_session_from_services(
    options: &CreateAgentSessionFromServicesOptions,
) -> anyhow::Result<CreateAgentSessionResult> {
    create_agent_session(Some(&CreateAgentSessionOptions {
        cwd: Some(options.services.cwd.clone()),
        agent_dir: Some(options.services.agent_dir.clone()),
        model_runtime: Some(options.services.model_runtime.clone()),
        settings_manager: Some(options.services.settings_manager.clone()),
        resource_loader: Some(options.services.resource_loader.clone()),
        session_manager: Some(options.session_manager.clone()),
        model: options.model.clone(),
        thinking_level: options.thinking_level,
        scoped_models: options.scoped_models.clone().map(|models| {
            models
                .into_iter()
                .map(|m| CreateAgentSessionOptionsScopedModels {
                    model: m.model,
                    thinking_level: m.thinking_level,
                })
                .collect()
        }),
        tools: options.tools.clone(),
        exclude_tools: options.exclude_tools.clone(),
        no_tools: options.no_tools.map(|n| match n {
            CreateAgentSessionFromServicesOptionsNoTools::All => CreateAgentSessionOptionsNoTools::All,
            CreateAgentSessionFromServicesOptionsNoTools::Builtin => {
                CreateAgentSessionOptionsNoTools::Builtin
            }
        }),
        custom_tools: options.custom_tools.clone(),
        session_start_event: options.session_start_event.clone(),
    }))
    .await
}
