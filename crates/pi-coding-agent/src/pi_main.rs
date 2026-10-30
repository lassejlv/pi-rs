//! Port of `packages/coding-agent/src/main.ts`.
//!
//! Main entry point for the coding agent CLI.
//!
//! This file handles CLI argument parsing and translates them into
//! createAgentSession() options. The SDK does the heavy lifting.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::cli::args::{
    Args, ArgsDiagnosticsType, ArgsListModels, ArgsUnknownFlags, Mode, normalize_session_name, parse_args, print_help,
};
use crate::cli::auth_check::{
    AuthCheckReason, AuthCheckResult, AuthCheckStatus, CheckProviderAuthOptions, GetProviderCredentialOptions,
    check_provider_auth, create_auth_check_model_runtime, get_provider_credential,
};
use crate::cli::auth_command::{
    AuthCommandError, AuthCommandKind, get_auth_command_name, get_auth_command_usage, is_auth_command_help,
    parse_auth_command, print_auth_command_help, validate_auth_command_args,
};
use crate::cli::credential_print::{CredentialPrintKindValue, resolve_credential_for_print};
use crate::cli::file_processor::{ProcessFileOptions, process_file_arguments};
use crate::cli::initial_message::{InitialMessageInput, InitialMessageResult, build_initial_message};
use crate::cli::list_models::list_models;
use crate::cli::project_trust::{CreateProjectTrustContextOptions, create_project_trust_context};
use crate::cli::session_picker::{SessionsLoader, select_session};
use crate::cli::startup_ui::{
    ShowStartupSelectorOptions, should_run_first_time_setup, show_first_time_setup, show_startup_selector,
};
use crate::compat::AuthOperationOptions;
use crate::config::{APP_NAME, ENV_SESSION_DIR, VERSION, expand_tilde_path, get_agent_dir, get_package_dir};
use crate::core::agent_session_runtime::{
    CreateAgentSessionRuntimeFactory, CreateAgentSessionRuntimeFactoryOptions, CreateAgentSessionRuntimeOptions,
    CreateAgentSessionRuntimeResult, create_agent_session_runtime,
};
use crate::core::agent_session_services::{
    AgentSessionRuntimeDiagnostic, AgentSessionRuntimeDiagnosticType, CreateAgentSessionFromServicesOptions,
    CreateAgentSessionFromServicesOptionsNoTools, CreateAgentSessionFromServicesOptionsScopedModels,
    CreateAgentSessionServicesOptions, create_agent_session_from_services, create_agent_session_services,
};
use crate::core::auth_storage::{AuthStorage, ReadOnlyAuthStorage};
use crate::core::export_html::{ExportFromFileOptions, export_from_file};
use crate::core::extensions::types::{FlagValue, InlineExtension};
use crate::core::http_dispatcher::{apply_http_proxy_settings, configure_http_dispatcher};
use crate::core::model_resolver::{ResolveCliModelOptions, ScopedModel, resolve_cli_model, resolve_model_scope};
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use crate::core::output_guard::{restore_stdout, take_over_stdout};
use crate::core::project_trust::{AppMode, ResolveProjectTrustedOptions, resolve_project_trusted};
use crate::core::resource_loader::{
    DefaultResourceLoaderOptions, ResolveProjectTrustFn, ResourceLoaderReloadOptions,
};
use crate::core::session_cwd::{
    MissingSessionCwdError, SessionCwdIssue, SessionCwdSource, format_missing_session_cwd_prompt,
    get_missing_session_cwd_issue,
};
use crate::core::session_manager::{
    NewSessionOptions, SessionInfo, SessionListProgress, SessionManager, assert_valid_session_id,
};
use crate::core::settings_diagnostics::{collect_settings_diagnostics, deduplicate_diagnostics};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::settings_schema::Settings;
use crate::core::timings::{print_timings, reset_timings, time};
use crate::core::trust_manager::{ProjectTrustStore, has_trust_requiring_project_resources};
use crate::extensions::BUILT_IN_EXTENSIONS;
use crate::extensions::mcp::cli::{McpCommandOptions, run_mcp_command};
use crate::migrations::{run_migrations, show_deprecation_warnings};
use crate::modes::interactive::interactive_mode::{InteractiveMode, InteractiveModeOptions};
use crate::modes::interactive::theme::theme::{ThemeJsonValidator, init_theme, set_theme_json_validator, stop_theme_watcher};
use crate::modes::interactive::theme::theme_schema::validate_theme_json;
use crate::modes::print_mode::{PrintModeOptions, PrintModeOptionsMode, run_print_mode};
use crate::modes::rpc::rpc_mode::run_rpc_mode;
use crate::package_manager_cli::{
    PackageCommandRuntimeOptions, cleanup_managed_install, handle_config_command, handle_package_command,
};
use crate::utils::chalk;
use crate::utils::paths::{is_local_path, normalize_path, resolve_path};
use crate::utils::process_exit::{exit, exit_code, set_exit_code};
use crate::utils::windows_self_update::cleanup_windows_self_update_quarantine;

fn extension_load_failure_hint() -> String {
    format!("Hint: Start without extensions using \"{APP_NAME} -ne\".")
}

/// A token cancelled after `ms` milliseconds (`AbortSignal.timeout(ms)`).
fn timeout_signal(ms: u64) -> CancellationToken {
    let token = CancellationToken::new();
    let cancel = token.clone();
    crate::utils::uncaught::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        cancel.cancel();
    });
    token
}

/// Read all content from piped stdin.
/// Returns `None` if stdin is a TTY (interactive terminal).
async fn read_piped_stdin() -> Option<String> {
    // If stdin is a TTY, we're running interactively - don't read stdin
    if std::io::stdin().is_terminal() {
        return None;
    }
    let data = tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
    .await
    .unwrap_or_default();
    let trimmed = data.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn report_diagnostics(diagnostics: &[AgentSessionRuntimeDiagnostic]) {
    for diagnostic in diagnostics {
        let line = match diagnostic.r#type {
            AgentSessionRuntimeDiagnosticType::Error => chalk::red(&format!("Error: {}", diagnostic.message)),
            AgentSessionRuntimeDiagnosticType::Warning => chalk::yellow(&format!("Warning: {}", diagnostic.message)),
            AgentSessionRuntimeDiagnosticType::Info => chalk::dim(&diagnostic.message),
        };
        eprintln!("{line}");
    }
}

fn is_truthy_env_flag(value: Option<&str>) -> bool {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return false;
    };
    value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

fn resolve_app_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    match parsed.mode {
        Some(Mode::Rpc) => return AppMode::Rpc,
        Some(Mode::Json) => return AppMode::Json,
        _ => {}
    }
    if parsed.print == Some(true) || !stdin_is_tty || !stdout_is_tty {
        return AppMode::Print;
    }
    AppMode::Interactive
}

fn to_print_output_mode(app_mode: AppMode) -> PrintModeOptionsMode {
    if app_mode == AppMode::Json { PrintModeOptionsMode::Json } else { PrintModeOptionsMode::Text }
}

fn is_plain_runtime_metadata_command(parsed: &Args) -> bool {
    parsed.print != Some(true)
        && parsed.mode.is_none()
        && (parsed.help == Some(true) || parsed.list_models.is_some())
}

fn auth_error_message(error: &anyhow::Error, fallback: &str) -> String {
    match error.downcast_ref::<AuthCommandError>() {
        Some(error) => error.message.clone(),
        None => fallback.to_string(),
    }
}

async fn run_auth_command(args: &[String]) -> anyhow::Result<bool> {
    if is_auth_command_help(args) {
        print_auth_command_help();
        return Ok(true);
    }

    let command = match parse_auth_command(args) {
        Ok(Some(command)) => command,
        Ok(None) => return Ok(false),
        Err(error) => {
            eprintln!(
                "{}",
                chalk::red(&format!("Error: {}", auth_error_message(&error, "Failed to parse auth command")))
            );
            set_exit_code(1);
            return Ok(true);
        }
    };

    let parsed = parse_args(&command.args);
    if let Some(option) = parsed.unknown_flags.keys().next() {
        eprintln!(
            "{}",
            chalk::red(&format!("Unknown option --{option} for \"{}\".", get_auth_command_name(&command.kind)))
        );
        eprintln!(
            "{}",
            chalk::dim(&format!("Use \"{APP_NAME} --help\" or \"{}\".", get_auth_command_usage(&command.kind)))
        );
        set_exit_code(1);
        return Ok(true);
    }

    let outcome: anyhow::Result<()> = async {
        if !parsed.diagnostics.is_empty() {
            return Err(AuthCommandError::new(
                parsed.diagnostics.iter().map(|d| d.message.clone()).collect::<Vec<_>>().join("\n"),
            )
            .into());
        }
        if command.kind != AuthCommandKind::Check {
            let signal = timeout_signal(15_000);
            let model_runtime = ModelRuntime::create(Some(&CreateModelRuntimeOptions {
                credentials: None,
                auth_path: None,
                models_path: None,
                models_store: None,
                models_store_path: None,
                allow_model_network: Some(false),
                model_refresh_timeout_ms: None,
                catalog_base_url: None,
                signal: Some(signal.clone()),
                refresh_on_create: None,
            }))
            .await?;
            let kind = if command.kind == AuthCommandKind::ApiKey {
                CredentialPrintKindValue::ApiKey
            } else {
                CredentialPrintKindValue::BearerToken
            };
            let credential =
                resolve_credential_for_print(&parsed, model_runtime, &kind, command.min_expiry_ms, Some(signal)).await?;
            println!("{credential}");
            return Ok(());
        }

        let requested_auth = validate_auth_command_args(&parsed, &command.kind)?;
        let mut credential: Option<String> = None;
        let result: anyhow::Result<AuthCheckResult> = async {
            let credentials: Arc<dyn pi_ai::auth::types::CredentialStore> = if command.no_refresh {
                Arc::new(ReadOnlyAuthStorage::new(None))
            } else {
                AuthStorage::create(None)
            };
            let model_runtime = create_auth_check_model_runtime(credentials.clone()).await?;
            let mut result = check_provider_auth(
                &parsed,
                model_runtime.clone(),
                Some(&CheckProviderAuthOptions { refresh: !command.no_refresh }),
            )
            .await?;
            if command.credentials && result.status == AuthCheckStatus::Ready {
                credential = get_provider_credential(
                    &result.provider,
                    model_runtime,
                    credentials,
                    &GetProviderCredentialOptions { refresh: !command.no_refresh },
                )
                .await?;
                if credential.is_none() {
                    result = AuthCheckResult {
                        status: AuthCheckStatus::NotReady,
                        provider: result.provider,
                        reason: Some(AuthCheckReason::CredentialNotAvailable),
                        auth_type: None,
                    };
                }
            }
            Ok(result)
        }
        .await;
        let result = result.unwrap_or_else(|_| AuthCheckResult {
            status: AuthCheckStatus::Invalid,
            provider: requested_auth.provider.clone().or(requested_auth.model.clone()).unwrap_or_default(),
            reason: Some(AuthCheckReason::InvalidState),
            auth_type: None,
        });
        let output = if command.json {
            let mut value = serde_json::to_value(&result)?;
            if let (Some(credential), Some(object)) = (&credential, value.as_object_mut()) {
                object.insert("credentials".to_string(), serde_json::Value::String(credential.clone()));
            }
            serde_json::to_string(&value)?
        } else {
            credential.clone().unwrap_or_else(|| result.status.as_str().to_string())
        };
        println!("{output}");
        set_exit_code(match result.status {
            AuthCheckStatus::Ready => 0,
            AuthCheckStatus::NotReady => 1,
            AuthCheckStatus::Invalid => 2,
        });
        Ok(())
    }
    .await;

    if let Err(error) = outcome {
        eprintln!("{}", chalk::red(&format!("Error: {}", auth_error_message(&error, "Failed to resolve credential"))));
        set_exit_code(if command.kind == AuthCommandKind::Check { 2 } else { 1 });
    }
    Ok(true)
}

async fn prepare_initial_message(parsed: &mut Args, stdin_content: Option<String>) -> anyhow::Result<InitialMessageResult> {
    if parsed.file_args.is_empty() {
        return Ok(build_initial_message(InitialMessageInput {
            parsed,
            file_text: None,
            file_images: None,
            stdin_content,
        }));
    }

    // AgentSession resizes these after extension hooks select the request model.
    let processed =
        process_file_arguments(&parsed.file_args, Some(&ProcessFileOptions { auto_resize_images: Some(false) })).await?;
    Ok(build_initial_message(InitialMessageInput {
        parsed,
        file_text: Some(processed.text),
        file_images: Some(processed.images),
        stdin_content,
    }))
}

/// Result from resolving a session argument
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ResolvedSession {
    /// Direct file path
    #[serde(rename = "path")]
    Path { path: String },
    /// Found in current project
    #[serde(rename = "local")]
    Local { path: String },
    /// Found in different project
    #[serde(rename = "global")]
    Global { path: String, cwd: String },
    /// Not found anywhere
    #[serde(rename = "not_found")]
    NotFound { arg: String },
}

fn find_local_session_by_exact_id(session_id: &str, cwd: &str, session_dir: Option<&str>) -> Option<String> {
    SessionManager::find_by_id(cwd, session_id, session_dir).ok().flatten()
}

/// Resolve a session argument to a file path.
/// If it looks like a path, use as-is. Otherwise try to match as session ID prefix.
async fn resolve_session_path(session_arg: &str, cwd: &str, session_dir: Option<&str>) -> anyhow::Result<ResolvedSession> {
    // If it looks like a file path, resolve it before handing it to the session manager.
    if session_arg.contains('/') || session_arg.contains('\\') || session_arg.ends_with(".jsonl") {
        return Ok(ResolvedSession::Path { path: resolve_path(session_arg, Some(cwd), None) });
    }

    // Exact IDs only require reading session headers. Fall back to the full
    // metadata listing for prefix matches.
    if let Some(path) = find_local_session_by_exact_id(session_arg, cwd, session_dir) {
        return Ok(ResolvedSession::Local { path });
    }

    let local_sessions = SessionManager::list(cwd, session_dir, None, None).await?;
    if let Some(local_match) = local_sessions.iter().find(|s| s.id.starts_with(session_arg)) {
        return Ok(ResolvedSession::Local { path: local_match.path.clone() });
    }

    // Try global search across all projects
    let all_sessions = SessionManager::list_all(session_dir, None, None).await?;
    let global_match = all_sessions
        .iter()
        .find(|s| s.id == session_arg)
        .or_else(|| all_sessions.iter().find(|s| s.id.starts_with(session_arg)));
    if let Some(global_match) = global_match {
        return Ok(ResolvedSession::Global { path: global_match.path.clone(), cwd: global_match.cwd.clone() });
    }

    // Not found anywhere
    Ok(ResolvedSession::NotFound { arg: session_arg.to_string() })
}

/// Prompt user for yes/no confirmation
async fn prompt_confirm(message: &str) -> bool {
    use std::io::Write;
    print!("{message} [y/N] ");
    let _ = std::io::stdout().flush();
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .unwrap_or_default();
    let answer = answer.trim_end_matches(['\r', '\n']).to_lowercase();
    answer == "y" || answer == "yes"
}

fn validate_fork_flags(parsed: &Args) {
    if parsed.fork.is_none() {
        return;
    }
    let conflicting_flags: Vec<&str> = [
        parsed.session.as_ref().filter(|s| !s.is_empty()).map(|_| "--session"),
        (parsed.r#continue == Some(true)).then_some("--continue"),
        (parsed.resume == Some(true)).then_some("--resume"),
        (parsed.no_session == Some(true)).then_some("--no-session"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !conflicting_flags.is_empty() {
        eprintln!(
            "{}",
            chalk::red(&format!("Error: --fork cannot be combined with {}", conflicting_flags.join(", ")))
        );
        exit(1);
    }
}

fn validate_session_id_flags(parsed: &Args) {
    let Some(session_id) = &parsed.session_id else {
        return;
    };
    let conflicting_flags: Vec<&str> = [
        parsed.session.as_ref().filter(|s| !s.is_empty()).map(|_| "--session"),
        (parsed.r#continue == Some(true)).then_some("--continue"),
        (parsed.resume == Some(true)).then_some("--resume"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !conflicting_flags.is_empty() {
        eprintln!(
            "{}",
            chalk::red(&format!("Error: --session-id cannot be combined with {}", conflicting_flags.join(", ")))
        );
        exit(1);
    }
    if let Err(error) = assert_valid_session_id(session_id) {
        eprintln!("{}", chalk::red(&format!("Error: {error}")));
        exit(1);
    }
}

fn open_session_or_exit(path: &str, session_dir: Option<&str>) -> Arc<SessionManager> {
    match SessionManager::open(path, session_dir, None) {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("{}", chalk::red(&format!("Error: {error}")));
            exit(1);
        }
    }
}

fn fork_session_or_exit(
    source_path: &str,
    cwd: &str,
    session_dir: Option<&str>,
    session_id: Option<&str>,
) -> Arc<SessionManager> {
    let options = NewSessionOptions { id: session_id.map(str::to_string), parent_session: None };
    match SessionManager::fork_from(source_path, cwd, session_dir, Some(&options)) {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("{}", chalk::red(&format!("Error: {error}")));
            exit(1);
        }
    }
}

type SessionsFuture = BoxFuture<'static, anyhow::Result<Vec<SessionInfo>>>;

pub async fn create_session_manager(
    parsed: &Args,
    cwd: &str,
    session_dir: Option<&str>,
    settings_manager: Arc<SettingsManager>,
) -> anyhow::Result<Arc<SessionManager>> {
    if parsed.no_session == Some(true) || parsed.help == Some(true) || parsed.list_models.is_some() {
        let options = parsed.session_id.as_ref().map(|id| NewSessionOptions { id: Some(id.clone()), parent_session: None });
        return SessionManager::in_memory(Some(cwd), options.as_ref(), None);
    }

    if let Some(fork) = parsed.fork.as_ref().filter(|f| !f.is_empty()) {
        if let Some(session_id) = parsed.session_id.as_ref().filter(|s| !s.is_empty())
            && find_local_session_by_exact_id(session_id, cwd, session_dir).is_some()
        {
            eprintln!("{}", chalk::red(&format!("Session already exists with id '{session_id}'")));
            exit(1);
        }

        return match resolve_session_path(fork, cwd, session_dir).await? {
            ResolvedSession::Path { path } | ResolvedSession::Local { path } | ResolvedSession::Global { path, .. } => {
                Ok(fork_session_or_exit(&path, cwd, session_dir, parsed.session_id.as_deref()))
            }
            ResolvedSession::NotFound { arg } => {
                eprintln!("{}", chalk::red(&format!("No session found matching '{arg}'")));
                exit(1);
            }
        };
    }

    if let Some(session) = parsed.session.as_ref().filter(|s| !s.is_empty()) {
        return match resolve_session_path(session, cwd, session_dir).await? {
            ResolvedSession::Path { path } | ResolvedSession::Local { path } => {
                Ok(open_session_or_exit(&path, session_dir))
            }
            ResolvedSession::Global { path, cwd: session_cwd } => {
                println!("{}", chalk::yellow(&format!("Session found in different project: {session_cwd}")));
                let should_fork = prompt_confirm("Fork this session into current directory?").await;
                if !should_fork {
                    println!("{}", chalk::dim("Aborted."));
                    exit(0);
                }
                Ok(fork_session_or_exit(&path, cwd, session_dir, None))
            }
            ResolvedSession::NotFound { arg } => {
                eprintln!("{}", chalk::red(&format!("No session found matching '{arg}'")));
                exit(1);
            }
        };
    }

    if parsed.resume == Some(true) {
        let current_loader: SessionsLoader = {
            let cwd = cwd.to_string();
            let session_dir = session_dir.map(str::to_string);
            Arc::new(move |on_progress: Option<SessionListProgress>, signal: Option<CancellationToken>| -> SessionsFuture {
                let cwd = cwd.clone();
                let session_dir = session_dir.clone();
                Box::pin(async move { SessionManager::list(&cwd, session_dir.as_deref(), on_progress, signal).await })
            })
        };
        let all_loader: SessionsLoader = {
            let session_dir = session_dir.map(str::to_string);
            Arc::new(move |on_progress: Option<SessionListProgress>, signal: Option<CancellationToken>| -> SessionsFuture {
                let session_dir = session_dir.clone();
                Box::pin(async move { SessionManager::list_all(session_dir.as_deref(), on_progress, signal).await })
            })
        };
        let selected = select_session(&current_loader, &all_loader, settings_manager).await;
        stop_theme_watcher();
        let Some(selected_path) = selected? else {
            println!("{}", chalk::dim("No session selected"));
            exit(0);
        };
        return SessionManager::open(&selected_path, session_dir, None);
    }

    if parsed.r#continue == Some(true) {
        return SessionManager::continue_recent(cwd, session_dir);
    }

    if let Some(session_id) = parsed.session_id.as_ref().filter(|s| !s.is_empty()) {
        if let Some(path) = find_local_session_by_exact_id(session_id, cwd, session_dir) {
            return SessionManager::open(&path, session_dir, None);
        }
        eprintln!(
            "{}",
            chalk::yellow(&format!(
                "Warning: No project session found with id '{session_id}'; creating a new session with that id."
            ))
        );
    }

    SessionManager::create(
        cwd,
        session_dir,
        Some(&NewSessionOptions { id: parsed.session_id.clone(), parent_session: None }),
    )
}

/// Session options resolved from CLI flags.
#[derive(Clone, Default)]
struct CliSessionOptions {
    model: Option<pi_ai::types::Model>,
    thinking_level: Option<pi_agent::types::ThinkingLevel>,
    scoped_models: Option<Vec<CreateAgentSessionFromServicesOptionsScopedModels>>,
    tools: Option<Vec<String>>,
    exclude_tools: Option<Vec<String>>,
    no_tools: Option<CreateAgentSessionFromServicesOptionsNoTools>,
}

fn diagnostic(kind: AgentSessionRuntimeDiagnosticType, message: impl Into<String>) -> AgentSessionRuntimeDiagnostic {
    AgentSessionRuntimeDiagnostic { r#type: kind, message: message.into() }
}

fn build_session_options(
    parsed: &Args,
    scoped_models: &[ScopedModel],
    has_existing_session: bool,
    model_runtime: &Arc<ModelRuntime>,
    settings_manager: &SettingsManager,
) -> (CliSessionOptions, bool, Vec<AgentSessionRuntimeDiagnostic>) {
    let mut options = CliSessionOptions::default();
    let mut diagnostics = Vec::new();
    let mut cli_thinking_from_model = false;

    // Model from CLI
    // - supports --provider <name> --model <pattern>
    // - supports --model <provider>/<pattern>
    let provider = parsed.provider.as_ref().filter(|p| !p.is_empty());
    let model = parsed.model.as_ref().filter(|m| !m.is_empty());
    if let (Some(provider), None) = (provider, model) {
        diagnostics.push(diagnostic(
            AgentSessionRuntimeDiagnosticType::Error,
            format!("--provider requires --model (for example: --provider {provider} --model <pattern>)"),
        ));
    }
    if let Some(cli_model) = model {
        let resolved = resolve_cli_model(&ResolveCliModelOptions {
            cli_provider: provider.cloned(),
            cli_model: Some(cli_model.clone()),
            cli_thinking: parsed.thinking,
            inline_api_key: Some(parsed.api_key.as_ref().is_some_and(|k| !k.is_empty())),
            model_runtime: model_runtime.clone(),
        });
        if let Some(warning) = resolved.warning {
            diagnostics.push(diagnostic(AgentSessionRuntimeDiagnosticType::Warning, warning));
        }
        if let Some(error) = resolved.error {
            diagnostics.push(diagnostic(AgentSessionRuntimeDiagnosticType::Error, error));
        }
        if let Some(model) = resolved.model {
            options.model = Some(model);
            // Allow "--model <pattern>:<thinking>" as a shorthand.
            // Explicit --thinking still takes precedence (applied later).
            if parsed.thinking.is_none()
                && let Some(level) = resolved.thinking_level
            {
                options.thinking_level = Some(level);
                cli_thinking_from_model = true;
            }
        }
    }

    if options.model.is_none() && !scoped_models.is_empty() && !has_existing_session {
        // Check if saved default is in scoped models - use it if so, otherwise first scoped model
        let saved_provider = settings_manager.get_default_provider();
        let saved_model_id = settings_manager.get_default_model();
        let saved_model = match (saved_provider, saved_model_id) {
            (Some(provider), Some(model_id)) if !provider.is_empty() && !model_id.is_empty() => {
                model_runtime.get_model(&provider, &model_id)
            }
            _ => None,
        };
        let saved_in_scope = saved_model.as_ref().and_then(|saved| {
            scoped_models.iter().find(|sm| sm.model.provider == saved.provider && sm.model.id == saved.id)
        });
        let chosen = saved_in_scope.unwrap_or(&scoped_models[0]);
        options.model = Some(chosen.model.clone());
        // Use thinking level from scoped model config if explicitly set
        if parsed.thinking.is_none()
            && let Some(level) = chosen.thinking_level
        {
            options.thinking_level = Some(level);
        }
    }

    // Thinking level from CLI (takes precedence over scoped model thinking levels set above)
    if let Some(thinking) = parsed.thinking {
        options.thinking_level = Some(thinking);
    }

    // Scoped models for Ctrl+P cycling
    // Keep thinking level undefined when not explicitly set in the model pattern.
    // Undefined means "inherit current session thinking level" during cycling.
    if !scoped_models.is_empty() {
        options.scoped_models = Some(
            scoped_models
                .iter()
                .map(|sm| CreateAgentSessionFromServicesOptionsScopedModels {
                    model: sm.model.clone(),
                    thinking_level: sm.thinking_level,
                })
                .collect(),
        );
    }

    // Tools
    if parsed.no_tools == Some(true) {
        options.no_tools = Some(CreateAgentSessionFromServicesOptionsNoTools::All);
    } else if parsed.no_builtin_tools == Some(true) {
        options.no_tools = Some(CreateAgentSessionFromServicesOptionsNoTools::Builtin);
    }
    if let Some(tools) = &parsed.tools {
        options.tools = Some(tools.clone());
    }
    if let Some(exclude_tools) = &parsed.exclude_tools {
        options.exclude_tools = Some(exclude_tools.clone());
    }

    (options, cli_thinking_from_model, diagnostics)
}

// Explicit tool requests must still work when extension discovery is disabled.
fn requested_builtin_tool_extensions(tools: Option<&Vec<String>>) -> Vec<String> {
    [("codemode", "codemode"), ("tool_search", "tool-search")]
        .into_iter()
        .filter(|(name, _)| {
            tools.is_some_and(|tools| {
                tools
                    .iter()
                    .rev()
                    .find_map(|entry| {
                        if entry == *name || entry.strip_prefix('+') == Some(*name) {
                            Some(true)
                        } else if entry.strip_prefix('-') == Some(*name) {
                            Some(false)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false)
            })
        })
        .map(|(_, extension)| format!("builtin:{extension}"))
        .collect()
}

#[cfg(test)]
mod builtin_tool_request_tests {
    use super::requested_builtin_tool_extensions;

    #[test]
    fn explicit_builtin_tools_are_loaded_even_without_discovery() {
        let tools = vec!["+codemode".into(), "+tool_search".into()];
        assert_eq!(
            requested_builtin_tool_extensions(Some(&tools)),
            ["builtin:codemode", "builtin:tool-search"]
        );
        let tools = vec!["+codemode".into(), "-codemode".into()];
        assert!(requested_builtin_tool_extensions(Some(&tools)).is_empty());
        assert!(requested_builtin_tool_extensions(None).is_empty());
    }
}

fn resolve_cli_paths(cwd: &str, paths: Option<&Vec<String>>) -> Option<Vec<String>> {
    paths.map(|paths| {
        paths
            .iter()
            .map(|value| if is_local_path(value) { resolve_path(value, Some(cwd), None) } else { value.clone() })
            .collect()
    })
}

async fn prompt_for_missing_session_cwd(
    issue: &SessionCwdIssue,
    settings_manager: Arc<SettingsManager>,
) -> anyhow::Result<Option<String>> {
    let selected = show_startup_selector(
        settings_manager,
        &format_missing_session_cwd_prompt(issue),
        &[
            ShowStartupSelectorOptions {
                label: "Continue".into(),
                value: serde_json::Value::String(issue.fallback_cwd.clone()),
            },
            ShowStartupSelectorOptions { label: "Cancel".into(), value: serde_json::Value::Null },
        ],
    )
    .await?;
    Ok(selected.and_then(|v| v.as_str().map(str::to_string)))
}

struct SessionManagerCwd<'a>(&'a SessionManager);

impl SessionCwdSource for SessionManagerCwd<'_> {
    fn get_cwd(&self) -> String {
        self.0.get_cwd()
    }
    fn get_session_file(&self) -> Option<String> {
        self.0.get_session_file()
    }
}

#[derive(Clone, Default)]
pub struct MainOptions {
    pub extension_factories: Option<Vec<InlineExtension>>,
}

fn current_dir() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".to_string())
}

fn to_flag_values(flags: &IndexMap<String, ArgsUnknownFlags>) -> IndexMap<String, FlagValue> {
    flags
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ArgsUnknownFlags::Bool(b) => FlagValue::Bool(*b),
                ArgsUnknownFlags::String(s) => FlagValue::String(s.clone()),
            };
            (name.clone(), value)
        })
        .collect()
}

pub async fn main(args: &[String], options: Option<&MainOptions>) -> anyhow::Result<()> {
    reset_timings(None);
    // Commands and startup selectors can load user-authored themes before runtime creation.
    let validator: ThemeJsonValidator = Arc::new(|label: String, json| validate_theme_json(&label, json));
    set_theme_json_validator(&validator);
    let mut extension_factories: Vec<InlineExtension> = BUILT_IN_EXTENSIONS.clone();
    if let Some(extra) = options.and_then(|o| o.extension_factories.clone()) {
        extension_factories.extend(extra);
    }
    let offline_mode =
        args.iter().any(|a| a == "--offline") || is_truthy_env_flag(std::env::var("PI_OFFLINE").ok().as_deref());
    if offline_mode {
        // SAFETY: set during startup before worker threads read these variables.
        unsafe {
            std::env::set_var("PI_OFFLINE", "1");
            std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
        }
    }

    if run_auth_command(args).await? {
        return Ok(());
    }

    if cfg!(windows) {
        cleanup_windows_self_update_quarantine(&get_package_dir());
    }
    cleanup_managed_install();

    let cwd = current_dir();
    let agent_dir = get_agent_dir();
    let bootstrap_settings_manager = SettingsManager::create(
        &cwd,
        Some(&agent_dir),
        Some(&SettingsManagerCreateOptions { project_trusted: Some(false) }),
    );
    apply_http_proxy_settings(bootstrap_settings_manager.get_global_settings().http_proxy.as_deref());
    configure_http_dispatcher(None)?;

    let package_runtime_options =
        PackageCommandRuntimeOptions { extension_factories: Some(extension_factories.clone()) };
    if handle_package_command(args, Some(&package_runtime_options)).await? {
        let code = exit_code().unwrap_or(0);
        if cfg!(windows) && code == 0 && args.first().map(String::as_str) == Some("update") {
            // Let a successful Windows `pi update` drain naturally.
            return Ok(());
        }
        exit(code);
    }

    if handle_config_command(args, Some(&package_runtime_options)).await? {
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("mcp") {
        let code = run_mcp_command(
            &args[1..],
            &McpCommandOptions {
                cwd: cwd.clone(),
                agent_dir: agent_dir.clone(),
                credentials: None,
                open_url: None,
                log: None,
                error: None,
            },
        )
        .await?;
        set_exit_code(code);
        return Ok(());
    }

    let mut parsed = parse_args(args);
    if !parsed.diagnostics.is_empty() {
        for d in &parsed.diagnostics {
            let line = if d.r#type == ArgsDiagnosticsType::Error {
                chalk::red(&format!("Error: {}", d.message))
            } else {
                chalk::yellow(&format!("Warning: {}", d.message))
            };
            eprintln!("{line}");
        }
        if parsed.diagnostics.iter().any(|d| d.r#type == ArgsDiagnosticsType::Error) {
            exit(1);
        }
    }
    time("parseArgs", None);

    if parsed.version == Some(true) {
        println!("{VERSION}");
        exit(0);
    }

    if let Some(export) = parsed.export.clone() {
        let output_path = parsed.messages.first().cloned();
        match export_from_file(&export, output_path.map(ExportFromFileOptions::String)).await {
            Ok(result) => {
                println!("Exported to: {result}");
                exit(0);
            }
            Err(error) => {
                let message = error.to_string();
                let message = if message.is_empty() { "Failed to export session".to_string() } else { message };
                eprintln!("{}", chalk::red(&format!("Error: {message}")));
                exit(1);
            }
        }
    }

    let mut app_mode = resolve_app_mode(&parsed, std::io::stdin().is_terminal(), std::io::stdout().is_terminal());
    let should_take_over_stdout = app_mode != AppMode::Interactive && !is_plain_runtime_metadata_command(&parsed);
    if should_take_over_stdout {
        take_over_stdout();
    }

    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        eprintln!("{}", chalk::red("Error: @file arguments are not supported in RPC mode"));
        exit(1);
    }

    validate_fork_flags(&parsed);
    validate_session_id_flags(&parsed);

    // Run migrations (pass cwd for project-local migrations)
    let migrations = run_migrations(&cwd);
    let migrated_providers = migrations.migrated_auth_providers;
    let deprecation_warnings = migrations.deprecation_warnings;
    time("runMigrations", None);

    let startup_settings_manager = SettingsManager::create(&cwd, Some(&agent_dir), None);
    let startup_settings_diagnostics = collect_settings_diagnostics(startup_settings_manager.clone());

    // Experimental first-time setup: theme choice and analytics opt-in.
    // Runs before any runtime services are created so the chosen settings apply everywhere.
    if app_mode == AppMode::Interactive
        && parsed.help != Some(true)
        && parsed.list_models.is_none()
        && should_run_first_time_setup(None)
    {
        show_first_time_setup(startup_settings_manager.clone()).await?;
        time("firstTimeSetup", None);
    }

    if app_mode == AppMode::Interactive
        && let Some(use_theme) = &parsed.use_theme
    {
        startup_settings_manager.apply_overrides(&Settings { theme: Some(use_theme.clone()), ..Default::default() });
    }

    // Decide the final runtime cwd before creating cwd-bound runtime services.
    // --session and --resume may select a session from another project, so project-local
    // settings, resources, provider registrations, and models must be resolved only after
    // the target session cwd is known. The startup-cwd settings manager is used only for
    // sessionDir lookup during session selection.
    let env_session_dir = std::env::var(ENV_SESSION_DIR).ok().filter(|v| !v.is_empty());
    let session_dir: Option<String> = parsed
        .session_dir
        .as_ref()
        .filter(|d| !d.is_empty())
        .map(|d| normalize_path(d, None))
        .or_else(|| env_session_dir.map(|d| expand_tilde_path(&d)))
        .or_else(|| startup_settings_manager.get_session_dir());
    let mut session_manager =
        create_session_manager(&parsed, &cwd, session_dir.as_deref(), startup_settings_manager.clone()).await?;
    if let Some(issue) = get_missing_session_cwd_issue(&SessionManagerCwd(&session_manager), &cwd) {
        if app_mode == AppMode::Interactive {
            let Some(selected_cwd) = prompt_for_missing_session_cwd(&issue, startup_settings_manager.clone()).await?
            else {
                exit(0);
            };
            session_manager = SessionManager::open(
                issue.session_file.as_deref().unwrap_or_default(),
                session_dir.as_deref(),
                Some(&selected_cwd),
            )?;
        } else {
            eprintln!("{}", chalk::red(&MissingSessionCwdError::new(&issue).message));
            exit(1);
        }
    }
    if let Some(name) = &parsed.name {
        let Some(name) = normalize_session_name(name) else {
            eprintln!("{}", chalk::red("Error: --name requires a non-empty value"));
            exit(1);
        };
        session_manager.append_session_info(&name)?;
    }
    time("createSessionManager", None);

    let trust_store = Arc::new(ProjectTrustStore::new(&agent_dir));
    let session_cwd = session_manager.get_cwd();
    let auto_trust_on_reload_cwd = (parsed.project_trust_override.is_none()
        && !has_trust_requiring_project_resources(&session_cwd))
    .then(|| session_cwd.clone());
    let trust_prompt_mode =
        if parsed.help == Some(true) || parsed.list_models.is_some() { AppMode::Print } else { app_mode };
    let project_trust_by_cwd: Arc<Mutex<HashMap<String, bool>>> = Arc::default();

    let mut resolved_extension_paths = resolve_cli_paths(&cwd, parsed.extensions.as_ref());
    if parsed.no_extensions == Some(true) {
        let paths = resolved_extension_paths.get_or_insert_with(Vec::new);
        for path in requested_builtin_tool_extensions(parsed.tools.as_ref()) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let resolved_skill_paths = resolve_cli_paths(&cwd, parsed.skills.as_ref());
    let resolved_prompt_template_paths = resolve_cli_paths(&cwd, parsed.prompt_templates.as_ref());
    let resolved_theme_paths = resolve_cli_paths(&cwd, parsed.themes.as_ref());

    let create_runtime: CreateAgentSessionRuntimeFactory = {
        let parsed = Arc::new(parsed.clone());
        let trust_store = trust_store.clone();
        let startup_settings_manager = startup_settings_manager.clone();
        let project_trust_by_cwd = project_trust_by_cwd.clone();
        let extension_factories = extension_factories.clone();
        Arc::new(move |factory_options: CreateAgentSessionRuntimeFactoryOptions| {
            let parsed = parsed.clone();
            let trust_store = trust_store.clone();
            let startup_settings_manager = startup_settings_manager.clone();
            let project_trust_by_cwd = project_trust_by_cwd.clone();
            let extension_factories = extension_factories.clone();
            let resolved_extension_paths = resolved_extension_paths.clone();
            let resolved_skill_paths = resolved_skill_paths.clone();
            let resolved_prompt_template_paths = resolved_prompt_template_paths.clone();
            let resolved_theme_paths = resolved_theme_paths.clone();
            let fut: BoxFuture<'static, anyhow::Result<CreateAgentSessionRuntimeResult>> = Box::pin(async move {
                let CreateAgentSessionRuntimeFactoryOptions {
                    cwd,
                    agent_dir,
                    session_manager,
                    session_start_event,
                    project_trust_context,
                } = factory_options;
                let is_initial_runtime = session_start_event.is_none();
                let project_trust_diagnostics: Arc<Mutex<Vec<AgentSessionRuntimeDiagnostic>>> = Arc::default();
                let cached_project_trust = project_trust_by_cwd.lock().get(&cwd).copied();
                let has_trust_requiring_resources = has_trust_requiring_project_resources(&cwd);
                let should_resolve_project_trust = parsed.project_trust_override.is_none()
                    && cached_project_trust.is_none()
                    && has_trust_requiring_resources;
                let project_trusted = if should_resolve_project_trust {
                    false
                } else {
                    cached_project_trust.or(parsed.project_trust_override).unwrap_or_else(|| {
                        !has_trust_requiring_resources || trust_store.get(&cwd).ok().flatten() == Some(true)
                    })
                };
                let runtime_settings_manager = SettingsManager::create(
                    &cwd,
                    Some(&agent_dir),
                    Some(&SettingsManagerCreateOptions { project_trusted: Some(project_trusted) }),
                );

                let resource_loader_reload_options = should_resolve_project_trust.then(|| {
                    let cwd = cwd.clone();
                    let trust_store = trust_store.clone();
                    let parsed = parsed.clone();
                    let startup_settings_manager = startup_settings_manager.clone();
                    let project_trust_context = project_trust_context.clone();
                    let project_trust_by_cwd = project_trust_by_cwd.clone();
                    let project_trust_diagnostics = project_trust_diagnostics.clone();
                    let resolve: ResolveProjectTrustFn = Arc::new(move |input| {
                        let cwd = cwd.clone();
                        let trust_store = trust_store.clone();
                        let parsed = parsed.clone();
                        let startup_settings_manager = startup_settings_manager.clone();
                        let project_trust_context = project_trust_context.clone();
                        let project_trust_by_cwd = project_trust_by_cwd.clone();
                        let project_trust_diagnostics = project_trust_diagnostics.clone();
                        Box::pin(async move {
                            let mode = if is_initial_runtime { trust_prompt_mode } else { app_mode };
                            let context = project_trust_context.unwrap_or_else(|| {
                                create_project_trust_context(&CreateProjectTrustContextOptions {
                                    cwd: cwd.clone(),
                                    mode,
                                    settings_manager: startup_settings_manager.clone(),
                                    has_ui: is_initial_runtime && trust_prompt_mode == AppMode::Interactive,
                                })
                            });
                            let trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
                                cwd: cwd.clone(),
                                trust_store,
                                trust_override: parsed.project_trust_override,
                                default_project_trust: Some(startup_settings_manager.get_default_project_trust()),
                                extensions_result: Some(input.extensions_result),
                                project_trust_context: context,
                                on_extension_error: Some(Arc::new(move |message: String| {
                                    project_trust_diagnostics
                                        .lock()
                                        .push(diagnostic(AgentSessionRuntimeDiagnosticType::Warning, message));
                                })),
                            })
                            .await?;
                            project_trust_by_cwd.lock().insert(cwd, trusted);
                            Ok(trusted)
                        })
                    });
                    ResourceLoaderReloadOptions { resolve_project_trust: Some(resolve) }
                });

                let services = create_agent_session_services(&CreateAgentSessionServicesOptions {
                    cwd: cwd.clone(),
                    agent_dir: Some(agent_dir.clone()),
                    settings_manager: Some(runtime_settings_manager),
                    model_runtime: None,
                    model_runtime_signal: Some(timeout_signal(15_000)),
                    extension_flag_values: Some(to_flag_values(&parsed.unknown_flags)),
                    resource_loader_reload_options,
                    resource_loader_options: Some(DefaultResourceLoaderOptions {
                        cwd: cwd.clone(),
                        agent_dir: agent_dir.clone(),
                        additional_extension_paths: resolved_extension_paths,
                        additional_skill_paths: resolved_skill_paths,
                        additional_prompt_template_paths: resolved_prompt_template_paths,
                        additional_theme_paths: resolved_theme_paths,
                        no_extensions: parsed.no_extensions,
                        disabled_builtin_extensions: (parsed.no_mcp == Some(true)).then(|| vec!["mcp".to_string()]),
                        no_skills: parsed.no_skills,
                        no_prompt_templates: parsed.no_prompt_templates,
                        no_themes: parsed.no_themes,
                        no_context_files: parsed.no_context_files,
                        system_prompt: parsed.system_prompt.clone(),
                        append_system_prompt: parsed.append_system_prompt.clone(),
                        extension_factories: Some(extension_factories),
                        ..Default::default()
                    }),
                })
                .await?;
                let settings_manager = services.settings_manager.clone();
                let model_runtime = services.model_runtime.clone();
                let resource_loader = services.resource_loader.clone();
                let extensions = resource_loader.get_extensions();
                let mut diagnostics: Vec<AgentSessionRuntimeDiagnostic> =
                    project_trust_diagnostics.lock().drain(..).collect();
                diagnostics.extend(services.diagnostics.iter().cloned());
                diagnostics.extend(collect_settings_diagnostics(settings_manager.clone()));
                diagnostics.extend(extensions.errors.iter().map(|e| {
                    diagnostic(
                        AgentSessionRuntimeDiagnosticType::Error,
                        format!("Failed to load extension \"{}\": {}", e.path, e.error),
                    )
                }));
                diagnostics.extend(extensions.warnings.iter().flatten().map(|w| {
                    diagnostic(
                        AgentSessionRuntimeDiagnosticType::Warning,
                        format!("Extension package \"{}\": {}", w.path, w.warning),
                    )
                }));

                let model_patterns = parsed.models.clone().or_else(|| settings_manager.get_enabled_models());
                let scoped_models = match model_patterns.filter(|p| !p.is_empty()) {
                    Some(patterns) => {
                        resolve_model_scope(
                            &patterns,
                            model_runtime.clone(),
                            Some(&AuthOperationOptions { signal: Some(timeout_signal(15_000)) }),
                        )
                        .await?
                    }
                    None => Vec::new(),
                };
                let (session_options, cli_thinking_from_model, session_option_diagnostics) = build_session_options(
                    &parsed,
                    &scoped_models,
                    !session_manager.build_session_context().messages.is_empty(),
                    &model_runtime,
                    &settings_manager,
                );
                diagnostics.extend(session_option_diagnostics);

                if let Some(api_key) = parsed.api_key.as_ref().filter(|k| !k.is_empty()) {
                    match &session_options.model {
                        None => diagnostics.push(diagnostic(
                            AgentSessionRuntimeDiagnosticType::Error,
                            "--api-key requires a model to be specified via --model, --provider/--model, or --models",
                        )),
                        Some(model) => model_runtime.set_runtime_api_key(&model.provider, api_key, None).await?,
                    }
                }

                let created = create_agent_session_from_services(&CreateAgentSessionFromServicesOptions {
                    services: services.clone(),
                    session_manager,
                    session_start_event,
                    model: session_options.model,
                    thinking_level: session_options.thinking_level,
                    scoped_models: session_options.scoped_models,
                    tools: session_options.tools,
                    exclude_tools: session_options.exclude_tools,
                    no_tools: session_options.no_tools,
                    custom_tools: None,
                })
                .await?;
                let cli_thinking_override = parsed.thinking.is_some() || cli_thinking_from_model;
                if created.session.model().is_some() && cli_thinking_override {
                    created.session.set_thinking_level(created.session.thinking_level(), None);
                }

                Ok(CreateAgentSessionRuntimeResult {
                    session: created.session,
                    extensions_result: created.extensions_result,
                    model_fallback_message: created.model_fallback_message,
                    services,
                    diagnostics,
                })
            });
            fut
        })
    };
    time("createRuntime", None);
    let runtime = create_agent_session_runtime(
        &create_runtime,
        &CreateAgentSessionRuntimeOptions {
            cwd: session_manager.get_cwd(),
            agent_dir: agent_dir.clone(),
            session_manager: session_manager.clone(),
            session_start_event: None,
        },
    )
    .await?;
    time("createAgentSessionRuntime", None);
    let services = runtime.services();
    let model_fallback_message = runtime.model_fallback_message();
    let settings_manager = services.settings_manager.clone();
    let model_runtime = services.model_runtime.clone();
    let resource_loader = services.resource_loader.clone();
    pi_tui::terminal_image::set_capability_overrides(settings_manager.get_terminal_capability_overrides());
    apply_http_proxy_settings(settings_manager.get_global_settings().http_proxy.as_deref());
    configure_http_dispatcher(Some(settings_manager.get_http_idle_timeout_ms()?))?;

    let runtime_diagnostics = runtime.diagnostics();
    let mut all_diagnostics = startup_settings_diagnostics.clone();
    all_diagnostics.extend(runtime_diagnostics.iter().cloned());
    let startup_diagnostics = deduplicate_diagnostics(&all_diagnostics);
    let has_runtime_errors = runtime_diagnostics.iter().any(|d| d.r#type == AgentSessionRuntimeDiagnosticType::Error);

    if parsed.help == Some(true) {
        report_diagnostics(&startup_diagnostics);
        let extension_flags: Vec<_> = resource_loader
            .get_extensions()
            .extensions
            .iter()
            .flat_map(|extension| extension.read().flags.values().cloned().collect::<Vec<_>>())
            .collect();
        print_help(Some(&extension_flags));
        exit(0);
    }

    if let Some(list_models_arg) = &parsed.list_models {
        report_diagnostics(&startup_diagnostics);
        let search_pattern = match list_models_arg {
            ArgsListModels::String(pattern) => Some(pattern.as_str()),
            ArgsListModels::Bool(_) => None,
        };
        list_models(model_runtime.clone(), search_pattern, Some(timeout_signal(15_000))).await?;
        exit(0);
    }

    // Read piped stdin content (if any) - skip for RPC mode which uses stdin for JSON-RPC
    let mut stdin_content: Option<String> = None;
    if app_mode != AppMode::Rpc {
        stdin_content = read_piped_stdin().await;
        if stdin_content.is_some() && app_mode == AppMode::Interactive {
            app_mode = AppMode::Print;
        }
    }
    time("readPipedStdin", None);

    let initial = prepare_initial_message(&mut parsed, stdin_content).await?;
    time("prepareInitialMessage", None);
    init_theme(settings_manager.get_theme().as_deref(), Some(app_mode == AppMode::Interactive));
    time("initTheme", None);

    // Show deprecation warnings in interactive mode
    if app_mode == AppMode::Interactive && !deprecation_warnings.is_empty() {
        show_deprecation_warnings(&deprecation_warnings).await?;
    }

    time("resolveModelScope", None);
    if app_mode != AppMode::Interactive || has_runtime_errors {
        report_diagnostics(&startup_diagnostics);
    }
    if has_runtime_errors {
        if runtime_diagnostics.iter().any(|d| d.message.contains("Failed to load extension")) {
            eprintln!("{}", chalk::yellow(&extension_load_failure_hint()));
        }
        exit(1);
    }
    time("createAgentSession", None);

    // TS checks `!session.model` here, but the TS agent always reports a model (a placeholder when
    // none was selected), so the check never fires: print and JSON modes fail on the first prompt
    // and RPC mode starts (an extension or client can still select a model).

    let startup_benchmark = is_truthy_env_flag(std::env::var("PI_STARTUP_BENCHMARK").ok().as_deref());
    if startup_benchmark && app_mode != AppMode::Interactive {
        eprintln!("{}", chalk::red("Error: PI_STARTUP_BENCHMARK only supports interactive mode"));
        exit(1);
    }

    // RPC refreshes catalogs here in the background; interactive mode starts its refresh after TUI initialization.
    if !offline_mode && app_mode == AppMode::Rpc {
        let model_runtime = model_runtime.clone();
        crate::utils::uncaught::spawn(async move {
            let _ = model_runtime
                .refresh(Some(pi_ai::models::ModelsRefreshOptions {
                    signal: Some(timeout_signal(15_000)),
                    ..Default::default()
                }))
                .await;
        });
    }

    match app_mode {
        AppMode::Rpc => {
            print_timings();
            run_rpc_mode(runtime).await?;
        }
        AppMode::Interactive => {
            // Install the app keybindings into pi-tui's global before any TUI component
            // (including extension views such as the MCP manager) reads them.
            pi_tui::keybindings::set_keybindings(
                crate::core::keybindings::KeybindingsManager::create(Some(&agent_dir)).tui_manager(),
            );
            let interactive_mode = InteractiveMode::new(
                runtime,
                Some(&InteractiveModeOptions {
                    migrated_providers: Some(migrated_providers),
                    startup_diagnostics: Some(startup_diagnostics),
                    model_fallback_message,
                    auto_trust_on_reload_cwd,
                    initial_message: initial.initial_message,
                    initial_images: initial.initial_images,
                    initial_messages: Some(parsed.messages.clone()),
                    verbose: parsed.verbose,
                    tui_mode: parsed.tui_mode,
                    initial_theme_setting: parsed.use_theme.clone(),
                    terminal: None,
                }),
            );
            if startup_benchmark {
                interactive_mode.init().await?;
                time("interactiveMode.init", None);
                // Give the TUI's stdin handler a brief chance to consume terminal query replies
                // (Kitty keyboard protocol, device attributes, cell size) before restoring the terminal.
                tokio::time::sleep(Duration::from_millis(150)).await;
                interactive_mode.stop(None);
                stop_theme_watcher();
                print_timings();
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                return Ok(());
            }

            print_timings();
            interactive_mode.run().await?;
        }
        AppMode::Print | AppMode::Json => {
            print_timings();
            let code = run_print_mode(
                runtime,
                &PrintModeOptions {
                    mode: to_print_output_mode(app_mode),
                    messages: Some(parsed.messages.clone()),
                    initial_message: initial.initial_message,
                    initial_images: initial.initial_images,
                },
            )
            .await?;
            stop_theme_watcher();
            restore_stdout();
            if code != 0 {
                set_exit_code(code);
            }
        }
    }
    Ok(())
}
