//! Port of `packages/coding-agent/src/package-manager-cli.ts`.
//!
//! `pi install`, `pi remove`/`uninstall`, `pi update`, `pi list` and `pi config`.
//! `process.exitCode` maps to [`crate::utils::process_exit::set_exit_code`].

use crate::cli::config_selector::{ConfigSelectorOptions, select_config};
use crate::cli::project_trust::{CreateProjectTrustContextOptions, create_project_trust_context};
use crate::config::{
    APP_NAME, CONFIG_DIR_NAME, InstallMethod, PACKAGE_NAME, SelfUpdateCommand, SelfUpdatePackageSpec,
    SelfUpdatePackageTarget, VERSION, detect_install_method, get_agent_dir, get_package_dir,
    get_self_update_command, get_self_update_unavailable_instruction,
};
use crate::core::extensions::types::InlineExtension;
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use crate::core::package_manager::{
    DefaultPackageManager, PackageManager, PackageManagerInstallAndPersistOptions,
    PackageManagerOptions, PackageManagerRemoveAndPersistOptions, ProgressCallback,
    ProgressEventType,
};
use crate::core::project_trust::{AppMode, ResolveProjectTrustedOptions, resolve_project_trusted};
use crate::core::resource_loader::{
    DefaultResourceLoader, DefaultResourceLoaderOptions, format_package_install_warning, is_builtin_extension,
};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions, SettingsScope};
use crate::core::trust_manager::{ProjectTrustStore, has_trust_requiring_project_resources};
use crate::modes::interactive::components::config_selector::ConfigWriteScope;
use crate::utils::chalk;
use crate::utils::lockfile::{LockError, LockOptions, lock_sync};
use crate::utils::paths::{canonicalize_path, get_cwd_relative_path, node_join, node_resolve};
use crate::utils::pi_user_agent::get_pi_user_agent;
use crate::utils::process_exit::{exit, set_exit_code};
use crate::utils::text::strip_bom;
use crate::utils::version_check::{
    GetLatestPiReleaseOptions, format_version_check_error, get_latest_pi_release,
    is_newer_package_version,
};
use crate::utils::windows_self_update::{
    cleanup_windows_self_update_quarantine, quarantine_windows_native_dependencies,
};
use anyhow::{anyhow, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageCommand {
    #[serde(rename = "install")]
    Install,
    #[serde(rename = "remove")]
    Remove,
    #[serde(rename = "update")]
    Update,
    #[serde(rename = "list")]
    List,
}

impl PackageCommand {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Update => "update",
            Self::List => "list",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum UpdateTarget {
    #[serde(rename = "all")]
    All {},
    #[serde(rename = "self")]
    SelfValue {},
    #[serde(rename = "extensions")]
    Extensions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    #[serde(rename = "models")]
    Models {},
}

const DEFAULT_INSTALLER_API_BASE: &str = "https://pi.dev/api/installer/releases";
const MANAGED_INSTALL_MARKER: &str = "managed-install.json";

/// `/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/`
fn is_managed_release_version(value: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$").unwrap()
    });
    RE.is_match(value)
}

// ---------------------------------------------------------------------------
// Console output (captured in tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    static CAPTURE: std::cell::RefCell<Option<(Vec<String>, Vec<String>)>> = const { std::cell::RefCell::new(None) };
}

/// `console.log`.
fn log(line: impl AsRef<str>) {
    #[cfg(test)]
    {
        let captured = CAPTURE.with(|c| {
            if let Some((out, _)) = c.borrow_mut().as_mut() {
                out.push(line.as_ref().to_string());
                true
            } else {
                false
            }
        });
        if captured {
            return;
        }
    }
    println!("{}", line.as_ref());
}

/// `console.error`.
fn error(line: impl AsRef<str>) {
    #[cfg(test)]
    {
        let captured = CAPTURE.with(|c| {
            if let Some((_, err)) = c.borrow_mut().as_mut() {
                err.push(line.as_ref().to_string());
                true
            } else {
                false
            }
        });
        if captured {
            return;
        }
    }
    eprintln!("{}", line.as_ref());
}

// ---------------------------------------------------------------------------
// Managed installs
// ---------------------------------------------------------------------------

fn get_active_managed_install_root() -> anyhow::Result<Option<String>> {
    let Some(configured_root) = std::env::var("PI_MANAGED_INSTALL_ROOT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };

    let managed_root = node_resolve(&[&configured_root]);
    let releases_dir = canonicalize_path(&node_join(&[&managed_root, "releases"]));
    // The launcher environment is inherited by child processes. Do not classify a
    // source checkout or another Pi installation launched from managed Pi as managed.
    if get_cwd_relative_path(&canonicalize_path(&get_package_dir()), &releases_dir).is_none() {
        return Ok(None);
    }

    let marker_path = node_join(&[&managed_root, MANAGED_INSTALL_MARKER]);
    let valid = std::fs::read_to_string(&marker_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&strip_bom(&text)).ok())
        .is_some_and(|marker| {
            marker.get("kind").and_then(|v| v.as_str()) == Some("pi-managed-install")
                && marker.get("schemaVersion").and_then(|v| v.as_f64()) == Some(1.0)
                && marker.get("layout").and_then(|v| v.as_str()) == Some("releases-v1")
        });
    if !valid {
        bail!("Managed install marker is missing or invalid: {marker_path}");
    }
    Ok(Some(managed_root))
}

async fn fetch_installer_artifact(url: &str, label: &str) -> anyhow::Result<String> {
    let response = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, get_pi_user_agent(VERSION))
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "Could not download managed installer {label} from {url}: HTTP {}",
            response.status().as_u16()
        );
    }
    Ok(response.text().await?)
}

async fn run_managed_npm_ci(stage_dir: &str) -> anyhow::Result<()> {
    let args: Vec<String> = [
        "ci",
        "--ignore-scripts",
        "--min-release-age=0",
        "--omit=dev",
        "--include=optional",
        "--no-fund",
        "--no-audit",
        "--loglevel=error",
        "--progress=false",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let status = crate::utils::child_process::spawn_process("npm", &args)
        .current_dir(stage_dir)
        .status()
        .await?;
    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        bail!("npm {} exited with code {code}", args.join(" "));
    }
    Ok(())
}

fn verify_managed_release(release_dir: &str, expected_version: &str) -> anyhow::Result<()> {
    let bin_name = if cfg!(windows) {
        format!("{APP_NAME}.cmd")
    } else {
        APP_NAME.to_string()
    };
    let bin_path = node_join(&[release_dir, "node_modules", ".bin", &bin_name]);
    let output = crate::utils::child_process::spawn_process_sync(&bin_path, &["--version".to_string()])
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let reason = if stderr.is_empty() {
                format!(
                    "exit code {}",
                    output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            } else {
                stderr
            };
            bail!("Could not verify managed Pi {expected_version}: {reason}");
        }
        Err(e) => bail!("Could not verify managed Pi {expected_version}: {e}"),
    };
    let installed_version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if installed_version != expected_version {
        bail!(
            "Managed Pi smoke test returned version {installed_version}; expected {expected_version}."
        );
    }
    Ok(())
}

fn activate_managed_release(managed_root: &str, version: &str) -> anyhow::Result<()> {
    let current_path = node_join(&[managed_root, "current-version"]);
    let temporary_path = node_join(&[
        managed_root,
        &format!(
            "current-version.tmp.{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ),
    ]);
    let result = std::fs::write(&temporary_path, format!("{version}\n"))
        .and_then(|_| std::fs::rename(&temporary_path, &current_path));
    let _ = std::fs::remove_file(&temporary_path);
    result?;
    Ok(())
}

/// Keep the active release and the one running this update, which other open
/// sessions likely still use and which allows rolling back by editing current-version.
fn prune_managed_releases(managed_root: &str, active_version: &str) {
    let releases_root = node_join(&[managed_root, "releases"]);
    let Ok(entries) = std::fs::read_dir(&releases_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == active_version || name == VERSION || !is_managed_release_version(&name) {
            continue;
        }
        // Files may be in use (e.g. loaded native modules on Windows); retry on the next update.
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

fn cleanup_managed_staging(managed_root: &str) {
    let staging_root = node_join(&[managed_root, "staging"]);
    // The staging directory does not exist yet or is not writable.
    let Ok(entries) = std::fs::read_dir(&staging_root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("update-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

pub fn cleanup_managed_install() {
    let Ok(Some(managed_root)) = get_active_managed_install_root() else {
        return;
    };
    // A live update owns the staging directory, or cleanup is unavailable.
    if let Ok(lock) = lock_sync(node_join(&[&managed_root, "update"]), &LockOptions::default()) {
        cleanup_managed_staging(&managed_root);
        lock.release();
    }
}

async fn run_managed_self_update(managed_root: &str, version: &str) -> anyhow::Result<()> {
    if !is_managed_release_version(version) {
        bail!("Invalid managed release version: {version}");
    }

    let lock = match lock_sync(node_join(&[managed_root, "update"]), &LockOptions::default()) {
        Ok(lock) => lock,
        Err(LockError::Locked) => bail!("Another managed Pi update is already running."),
        Err(e) => return Err(e.into()),
    };

    let mut stage_dir: Option<String> = None;
    let result: anyhow::Result<()> = async {
        cleanup_managed_staging(managed_root);
        let installer_api_base = std::env::var("PI_INSTALLER_API_BASE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_INSTALLER_API_BASE.to_string());
        let installer_api_base = installer_api_base.trim_end_matches('/');
        let encoded: String = url::form_urlencoded::byte_serialize(version.as_bytes()).collect();
        let release_url = format!("{installer_api_base}/{}", encoded.replace('+', "%20"));
        let staging_root = node_join(&[managed_root, "staging"]);
        let releases_root = node_join(&[managed_root, "releases"]);
        std::fs::create_dir_all(&releases_root)?;
        let release_dir = node_join(&[&releases_root, version]);
        if std::path::Path::new(&release_dir).exists() {
            verify_managed_release(&release_dir, version)?;
            activate_managed_release(managed_root, version)?;
            prune_managed_releases(managed_root, version);
            return Ok(());
        }

        std::fs::create_dir_all(&staging_root)?;
        let dir = node_join(&[
            &staging_root,
            &format!("update-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]),
        ]);
        std::fs::create_dir(&dir)?;
        stage_dir = Some(dir.clone());
        let package_json_url = format!("{release_url}/package.json");
        let package_lock_url = format!("{release_url}/package-lock.json");
        let (package_json, package_lock) = futures::try_join!(
            fetch_installer_artifact(&package_json_url, "package.json"),
            fetch_installer_artifact(&package_lock_url, "package-lock.json"),
        )?;
        std::fs::write(node_join(&[&dir, "package.json"]), package_json)?;
        std::fs::write(node_join(&[&dir, "package-lock.json"]), package_lock)?;

        run_managed_npm_ci(&dir).await?;
        verify_managed_release(&dir, version)?;
        std::fs::rename(&dir, &release_dir)?;
        activate_managed_release(managed_root, version)?;
        prune_managed_releases(managed_root, version);
        Ok(())
    }
    .await;
    if let Some(dir) = stage_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
    lock.release();
    result
}

// ---------------------------------------------------------------------------
// Help and parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageCommandOptions {
    pub command: PackageCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_target: Option<UpdateTarget>,
    pub show_extensions_skipped_note: bool,
    pub local: bool,
    pub force: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_trust_override: Option<bool>,
    pub help: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_option: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_argument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_option_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicting_options: Option<String>,
}

fn report_settings_errors(settings_manager: &SettingsManager, context: &str) {
    for settings_error in settings_manager.drain_errors() {
        let scope = settings_error.scope.as_str();
        error(chalk::yellow(&format!(
            "Warning ({context}, {scope} settings): {}",
            settings_error.error
        )));
        // TS prints `chalk.dim(error.stack)`: the `${name}: ${message}` line followed by the V8 stack
        // frames, which name the installation's JavaScript files and line numbers. The port prints
        // the first line only.
        error(chalk::dim(&crate::core::settings_manager::js_error_stack_header(&settings_error.error)));
    }
}

fn get_package_command_usage(command: PackageCommand) -> String {
    match command {
        PackageCommand::Install => format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]"),
        PackageCommand::Remove => format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]"),
        PackageCommand::Update => format!(
            "{APP_NAME} update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
        ),
        PackageCommand::List => format!("{APP_NAME} list [--approve|--no-approve]"),
    }
}

fn config_command_usage() -> String {
    format!("{APP_NAME} config [-l] [--approve|--no-approve]")
}

fn print_config_command_help() {
    log(format!(
        "{}
  {}

Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/{CONFIG_DIR_NAME}/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides ({CONFIG_DIR_NAME}/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l
",
        chalk::bold("Usage:"),
        config_command_usage()
    ));
}

fn print_package_command_help(command: PackageCommand) {
    let usage = chalk::bold("Usage:");
    let text = match command {
        PackageCommand::Install => format!(
            "{usage}
  {}

Install a package and add it to settings.

Options:
  -l, --local       Install project-locally ({CONFIG_DIR_NAME}/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  {APP_NAME} install npm:@foo/bar
  {APP_NAME} install git:github.com/user/repo
  {APP_NAME} install git:git@github.com:user/repo
  {APP_NAME} install https://github.com/user/repo
  {APP_NAME} install ssh://git@github.com/user/repo
  {APP_NAME} install ./local/path
",
            get_package_command_usage(PackageCommand::Install)
        ),
        PackageCommand::Remove => format!(
            "{usage}
  {}

Remove a package and its source from settings.
Alias: {APP_NAME} uninstall <source> [-l]

Options:
  -l, --local       Remove from project settings ({CONFIG_DIR_NAME}/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  {APP_NAME} remove npm:@foo/bar
  {APP_NAME} uninstall npm:@foo/bar
",
            get_package_command_usage(PackageCommand::Remove)
        ),
        PackageCommand::Update => format!(
            "{usage}
  {}

Update pi, installed packages, or model catalogs.

Options:
  --self                  Update pi only (default when no target is given)
  --extensions            Update installed packages only
  --models                Refresh model catalogs only
  --all                   Update pi and installed packages
  --extension <source>    Update one package only
  -a, --approve           Trust project-local files for this command
  -na, --no-approve       Ignore project-local files for this command
  --force                 Reinstall pi even if the current version is latest

Short forms:
  {APP_NAME} update                Update pi only
  {APP_NAME} update --all          Update pi and all extensions
  {APP_NAME} update --models       Refresh model catalogs only
  {APP_NAME} update <source>       Update one package
  {APP_NAME} update pi             Update pi only (self works as alias to pi)
",
            get_package_command_usage(PackageCommand::Update)
        ),
        PackageCommand::List => format!(
            "{usage}
  {}

List installed packages from user and project settings.

Options:
  -a, --approve      Trust project-local files for this command
  -na, --no-approve  Ignore project-local files for this command
",
            get_package_command_usage(PackageCommand::List)
        ),
    };
    log(text);
}

fn parse_package_command(args: &[String]) -> Option<PackageCommandOptions> {
    let (raw_command, rest) = args.split_first()?;
    let command = match raw_command.as_str() {
        "uninstall" | "remove" => PackageCommand::Remove,
        "install" => PackageCommand::Install,
        "update" => PackageCommand::Update,
        "list" => PackageCommand::List,
        _ => return None,
    };

    let mut local = false;
    let mut force = false;
    let mut project_trust_override = None;
    let mut help = false;
    let mut invalid_option: Option<String> = None;
    let mut invalid_argument: Option<String> = None;
    let mut missing_option_value: Option<String> = None;
    let mut conflicting_options: Option<String> = None;
    let mut source: Option<String> = None;
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut models_flag = false;
    let mut all_flag = false;
    let mut extension_flag_source: Option<String> = None;
    let is_update = command == PackageCommand::Update;

    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].as_str();
        let mut set_invalid = |arg: &str| {
            if invalid_option.is_none() {
                invalid_option = Some(arg.to_string());
            }
        };
        match arg {
            "-h" | "--help" => help = true,
            "-l" | "--local" => {
                if command == PackageCommand::Install || command == PackageCommand::Remove {
                    local = true;
                } else {
                    set_invalid(arg);
                }
            }
            "--self" if is_update => self_flag = true,
            "--extensions" if is_update => extensions_flag = true,
            "--models" if is_update => models_flag = true,
            "--all" if is_update => all_flag = true,
            "--force" if is_update => force = true,
            "--self" | "--extensions" | "--models" | "--all" | "--force" => set_invalid(arg),
            "--approve" | "-a" => project_trust_override = Some(true),
            "--no-approve" | "-na" => project_trust_override = Some(false),
            "--extension" => {
                if !is_update {
                    set_invalid(arg);
                } else {
                    let value = rest.get(index + 1);
                    match value {
                        Some(value) if !value.is_empty() && !value.starts_with('-') => {
                            if extension_flag_source.is_some() {
                                if conflicting_options.is_none() {
                                    conflicting_options =
                                        Some("--extension can only be provided once".to_string());
                                }
                            } else {
                                extension_flag_source = Some(value.clone());
                            }
                            index += 1;
                        }
                        _ => {
                            if missing_option_value.is_none() {
                                missing_option_value = Some(arg.to_string());
                            }
                        }
                    }
                }
            }
            _ if arg.starts_with('-') => set_invalid(arg),
            _ => {
                if source.is_none() {
                    source = Some(arg.to_string());
                } else if invalid_argument.is_none() {
                    invalid_argument = Some(arg.to_string());
                }
            }
        }
        index += 1;
    }

    let mut conflict = |message: &str| {
        if conflicting_options.is_none() {
            conflicting_options = Some(message.to_string());
        }
    };
    let mut update_target = None;
    let mut show_extensions_skipped_note = false;
    if is_update {
        if all_flag && (self_flag || extensions_flag || models_flag || extension_flag_source.is_some()) {
            conflict("--all cannot be combined with --self, --extensions, --models, or --extension");
        }
        if all_flag && source.is_some() {
            conflict("--all cannot be combined with a positional source");
        }

        if models_flag {
            if self_flag || extensions_flag || all_flag || extension_flag_source.is_some() {
                conflict("--models cannot be combined with --self, --extensions, --all, or --extension");
            }
            if source.is_some() {
                conflict("--models cannot be combined with a positional source");
            }
            update_target = Some(UpdateTarget::Models {});
        } else if let Some(extension_source) = &extension_flag_source {
            if self_flag || extensions_flag || all_flag {
                conflict("--extension cannot be combined with --self, --extensions, or --all");
            }
            if source.is_some() {
                conflict("--extension cannot be combined with a positional source");
            }
            update_target = Some(UpdateTarget::Extensions {
                source: Some(extension_source.clone()),
            });
        } else if let Some(source) = &source {
            if source == "self" || source == "pi" {
                update_target = Some(if extensions_flag {
                    UpdateTarget::All {}
                } else {
                    UpdateTarget::SelfValue {}
                });
            } else {
                if extensions_flag || self_flag || all_flag {
                    conflict("positional update targets cannot be combined with --self, --extensions, or --all");
                }
                update_target = Some(UpdateTarget::Extensions {
                    source: Some(source.clone()),
                });
            }
        } else if all_flag || (self_flag && extensions_flag) {
            update_target = Some(UpdateTarget::All {});
        } else if self_flag {
            update_target = Some(UpdateTarget::SelfValue {});
        } else if extensions_flag {
            update_target = Some(UpdateTarget::Extensions { source: None });
        } else {
            update_target = Some(UpdateTarget::SelfValue {});
            show_extensions_skipped_note = true;
        }
    }

    Some(PackageCommandOptions {
        command,
        source,
        update_target,
        show_extensions_skipped_note,
        local,
        force,
        project_trust_override,
        help,
        invalid_option,
        invalid_argument,
        missing_option_value,
        conflicting_options,
    })
}

fn update_target_includes_self(target: &UpdateTarget) -> bool {
    matches!(target, UpdateTarget::All {} | UpdateTarget::SelfValue {})
}

fn update_target_includes_extensions(target: &UpdateTarget) -> bool {
    matches!(target, UpdateTarget::All {} | UpdateTarget::Extensions { .. })
}

async fn refresh_model_catalogs(agent_dir: &str) -> anyhow::Result<()> {
    let signal = tokio_util::sync::CancellationToken::new();
    let timer_signal = signal.clone();
    let timer = crate::utils::uncaught::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(15_000)).await;
        timer_signal.cancel();
    });
    let result: anyhow::Result<()> = async {
        let model_runtime = ModelRuntime::create(Some(&CreateModelRuntimeOptions {
            auth_path: Some(node_join(&[agent_dir, "auth.json"])),
            models_path: Some(node_join(&[agent_dir, "models.json"])),
            allow_model_network: Some(false),
            signal: Some(signal.clone()),
            ..Default::default()
        }))
        .await?;
        let result = model_runtime
            .refresh(Some(pi_ai::models::ModelsRefreshOptions {
                allow_network: Some(true),
                force: Some(true),
                signal: Some(signal.clone()),
                providers: None,
            }))
            .await?;
        if result.aborted {
            bail!("Model catalog refresh timed out.");
        }
        if !result.errors.is_empty() {
            let details = result
                .errors
                .iter()
                .map(|(provider, error)| format!("{provider}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("Could not refresh model catalogs: {details}");
        }
        Ok(())
    }
    .await;
    timer.abort();
    result?;
    log(chalk::green("Model catalogs refreshed"));
    Ok(())
}

fn print_self_update_unavailable(
    npm_command: Option<&[String]>,
    update_package_target: &SelfUpdatePackageTarget,
) {
    error(format!("error: {APP_NAME} cannot self-update this installation."));
    error(get_self_update_unavailable_instruction(
        PACKAGE_NAME,
        npm_command,
        Some(update_package_target),
    ));
    if let Ok(entrypoint) = std::env::current_exe() {
        error("");
        error(format!(
            "Location of {APP_NAME} executable: {}",
            entrypoint.to_string_lossy()
        ));
    }
}

fn print_self_update_fallback(command: &SelfUpdateCommand) {
    error(chalk::dim(&format!(
        "If this keeps failing, run this command yourself: {}",
        command.display
    )));
}

fn print_pnpm_self_update_metadata_hint() {
    error(chalk::yellow(
        "If pnpm reports missing package versions, its cached registry metadata may be stale.",
    ));
    error(chalk::yellow(&format!(
        "Run `pnpm store prune` and retry `{APP_NAME} update --self`."
    )));
}

fn stdout_columns() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
        if ok && size.ws_col > 0 {
            return Some(size.ws_col as usize);
        }
    }
    None
}

fn self_update_note_markdown_theme() -> pi_tui::components::markdown::MarkdownTheme {
    use pi_tui::StyleFn;
    let f = |g: fn(&str) -> String| -> StyleFn { Arc::new(move |t: &str| g(t)) };
    let mut theme = pi_tui::components::markdown::MarkdownTheme::plain();
    theme.heading = Arc::new(|t: &str| chalk::bold(&chalk::yellow(t)));
    theme.link = f(chalk::cyan);
    theme.link_url = f(chalk::dim);
    theme.code = f(chalk::yellow);
    theme.code_block = f(chalk::dim);
    theme.code_block_border = f(chalk::dim);
    theme.quote = f(chalk::dim);
    theme.quote_border = f(chalk::dim);
    theme.hr = f(chalk::dim);
    theme.list_bullet = f(chalk::yellow);
    theme.bold = f(chalk::bold);
    theme.italic = Arc::new(|t: &str| if chalk::level() == 0 { t.to_string() } else { format!("\x1b[3m{t}\x1b[23m") });
    theme.strikethrough = Arc::new(|t: &str| if chalk::level() == 0 { t.to_string() } else { format!("\x1b[9m{t}\x1b[29m") });
    theme.underline = Arc::new(|t: &str| if chalk::level() == 0 { t.to_string() } else { format!("\x1b[4m{t}\x1b[24m") });
    theme
}

fn print_self_update_note(note: &str) {
    let trimmed_note = note.trim();
    if trimmed_note.is_empty() {
        return;
    }
    log("");
    log(chalk::bold(&chalk::yellow("Update note")));
    let width = stdout_columns().unwrap_or(80).max(20);
    let rendered = std::panic::catch_unwind(|| {
        use pi_tui::tui::Component;
        pi_tui::components::markdown::Markdown::new(
            trimmed_note,
            0,
            0,
            self_update_note_markdown_theme(),
            None,
            None,
        )
        .render(width)
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
    });
    match rendered {
        Ok(text) => log(text),
        Err(_) => log(trimmed_note),
    }
    log("");
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfUpdatePlan {
    pub package_name: String,
    pub install_spec: String,
    pub version: String,
    pub should_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

async fn get_self_update_plan(force: bool) -> anyhow::Result<SelfUpdatePlan> {
    let latest_release = get_latest_pi_release(
        VERSION,
        Some(&GetLatestPiReleaseOptions {
            timeout_ms: None,
            retry: Some(true),
        }),
    )
    .await
    .map_err(|error| {
        anyhow!(
            "Could not determine latest {APP_NAME} version: {}",
            format_version_check_error(&error)
        )
    })?;
    let Some(latest_release) = latest_release else {
        bail!("Could not determine latest {APP_NAME} version.");
    };

    let package_name = latest_release
        .package_name
        .clone()
        .unwrap_or_else(|| PACKAGE_NAME.to_string());
    let install_spec = format!("{package_name}@{}", latest_release.version);
    if force || package_name != PACKAGE_NAME || is_newer_package_version(&latest_release.version, VERSION)
    {
        return Ok(SelfUpdatePlan {
            package_name,
            install_spec,
            version: latest_release.version,
            note: latest_release.note,
            should_run: true,
        });
    }

    log(chalk::green(&format!("{APP_NAME} is already up to date (v{VERSION})")));
    Ok(SelfUpdatePlan {
        package_name,
        install_spec,
        version: latest_release.version,
        should_run: false,
        note: None,
    })
}

async fn run_self_update(command: &SelfUpdateCommand) -> anyhow::Result<()> {
    log(chalk::dim(&format!("Updating {APP_NAME} with {}...", command.display)));
    let steps: Vec<(String, Vec<String>, String)> = match &command.steps {
        Some(steps) => steps
            .iter()
            .map(|s| (s.command.clone(), s.args.clone(), s.display.clone()))
            .collect(),
        None => vec![(command.command.clone(), command.args.clone(), command.display.clone())],
    };
    for (step_command, step_args, display) in steps {
        let status = crate::utils::child_process::spawn_process(&step_command, &step_args)
            .status()
            .await?;
        if status.success() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                bail!("{display} terminated by signal {signal}");
            }
        }
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        bail!("{display} exited with code {code}");
    }
    Ok(())
}

fn prepare_windows_npm_self_update() {
    if !cfg!(windows) {
        return;
    }
    let package_dir = get_package_dir();
    cleanup_windows_self_update_quarantine(&package_dir);
    quarantine_windows_native_dependencies(&package_dir);
}

#[derive(Clone, Default)]
pub struct PackageCommandRuntimeOptions {
    pub extension_factories: Option<Vec<InlineExtension>>,
}

#[derive(Clone)]
pub struct CommandSettingsResult {
    pub settings_manager: Arc<SettingsManager>,
    pub project_trust_warnings: Vec<String>,
}

fn get_command_app_mode() -> AppMode {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        AppMode::Interactive
    } else {
        AppMode::Print
    }
}

fn report_project_trust_warnings(warnings: &[String]) {
    for warning in warnings {
        error(chalk::yellow(&format!("Warning: {warning}")));
    }
}

struct CreateCommandSettingsManagerOptions {
    cwd: String,
    agent_dir: String,
    project_trust_override: Option<bool>,
    use_saved_project_trust_only: bool,
    extension_factories: Option<Vec<InlineExtension>>,
}

async fn create_command_settings_manager(
    options: CreateCommandSettingsManagerOptions,
) -> anyhow::Result<CommandSettingsResult> {
    let settings_manager = SettingsManager::create(
        &options.cwd,
        Some(&options.agent_dir),
        Some(&SettingsManagerCreateOptions {
            project_trusted: Some(false),
        }),
    );
    let mut project_trust_warnings: Vec<String> = Vec::new();
    let trust_store = Arc::new(ProjectTrustStore::new(&options.agent_dir));
    if options.use_saved_project_trust_only {
        let saved_project_trusted = trust_store.get(&options.cwd).ok().flatten() == Some(true);
        settings_manager
            .set_project_trusted(options.project_trust_override.unwrap_or(saved_project_trusted));
        return Ok(CommandSettingsResult {
            settings_manager,
            project_trust_warnings,
        });
    }

    let app_mode = get_command_app_mode();
    let extensions_result = if options.project_trust_override.is_none()
        && has_trust_requiring_project_resources(&options.cwd)
    {
        let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
            cwd: options.cwd.clone(),
            agent_dir: options.agent_dir.clone(),
            settings_manager: Some(settings_manager.clone()),
            extension_factories: options.extension_factories.clone(),
            ..Default::default()
        });
        Some(loader.load_project_trust_extensions().await?)
    } else {
        None
    };
    for load_error in extensions_result.iter().flat_map(|r| r.errors.iter()) {
        project_trust_warnings.push(format!(
            "Failed to load extension \"{}\": {}",
            load_error.path, load_error.error
        ));
    }

    let extension_errors = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let sink = extension_errors.clone();
    let project_trusted = resolve_project_trusted(&ResolveProjectTrustedOptions {
        cwd: options.cwd.clone(),
        trust_store,
        trust_override: options.project_trust_override,
        default_project_trust: Some(settings_manager.get_default_project_trust()),
        extensions_result,
        project_trust_context: create_project_trust_context(&CreateProjectTrustContextOptions {
            cwd: options.cwd.clone(),
            mode: app_mode,
            settings_manager: settings_manager.clone(),
            has_ui: app_mode == AppMode::Interactive,
        }),
        on_extension_error: Some(Arc::new(move |message| sink.lock().push(message))),
    })
    .await?;
    project_trust_warnings.extend(extension_errors.lock().drain(..));
    settings_manager.set_project_trusted(project_trusted);
    Ok(CommandSettingsResult {
        settings_manager,
        project_trust_warnings,
    })
}

fn process_cwd() -> String {
    crate::utils::paths::current_dir_string()
}

pub async fn handle_config_command(
    args: &[String],
    runtime_options: Option<&PackageCommandRuntimeOptions>,
) -> anyhow::Result<bool> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(false);
    };
    if command != "config" {
        return Ok(false);
    }

    if rest.iter().any(|a| a == "-h" || a == "--help") {
        print_config_command_help();
        return Ok(true);
    }

    let mut local = false;
    let mut project_trust_override = None;
    for arg in rest {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => project_trust_override = Some(true),
            "-na" | "--no-approve" => project_trust_override = Some(false),
            _ if arg.starts_with('-') => {
                error(chalk::red(&format!("Unknown option {arg} for \"config\".")));
                error(chalk::dim(&format!(
                    "Use \"{APP_NAME} --help\" or \"{}\".",
                    config_command_usage()
                )));
                set_exit_code(1);
                return Ok(true);
            }
            _ => {
                error(chalk::red(&format!("Unexpected argument {arg}.")));
                error(chalk::dim(&format!("Usage: {}", config_command_usage())));
                set_exit_code(1);
                return Ok(true);
            }
        }
    }

    let cwd = process_cwd();
    let agent_dir = get_agent_dir();
    let extension_factories = runtime_options.and_then(|o| o.extension_factories.clone());
    let CommandSettingsResult {
        settings_manager,
        project_trust_warnings,
    } = create_command_settings_manager(CreateCommandSettingsManagerOptions {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        project_trust_override,
        use_saved_project_trust_only: false,
        extension_factories: extension_factories.clone(),
    })
    .await?;
    report_project_trust_warnings(&project_trust_warnings);
    if local && !settings_manager.is_project_trusted() {
        error(chalk::red(
            "Project is not trusted. Use --approve to modify local resource config.",
        ));
        set_exit_code(1);
        return Ok(true);
    }
    report_settings_errors(&settings_manager, "config command");
    let builtin_extensions: Vec<String> = extension_factories
        .unwrap_or_default()
        .iter()
        .filter(|input| is_builtin_extension(input))
        .filter_map(|input| match input {
            InlineExtension::Named(named) => Some(named.name.clone()),
            InlineExtension::Factory(_) => None,
        })
        .collect();
    let global_settings_manager = SettingsManager::create(
        &cwd,
        Some(&agent_dir),
        Some(&SettingsManagerCreateOptions {
            project_trusted: Some(false),
        }),
    );
    let global_resolved_paths = DefaultPackageManager::new(&PackageManagerOptions {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        settings_manager: global_settings_manager,
        builtin_extensions: Some(builtin_extensions.clone()),
    })
    .resolve(None)
    .await?;
    let project_resolved_paths = if settings_manager.is_project_trusted() {
        DefaultPackageManager::new(&PackageManagerOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            settings_manager: settings_manager.clone(),
            builtin_extensions: Some(builtin_extensions),
        })
        .resolve(None)
        .await?
    } else {
        global_resolved_paths.clone()
    };

    let mut install_warnings = std::collections::HashSet::new();
    for failure in global_resolved_paths
        .install_failures
        .iter()
        .flatten()
        .chain(project_resolved_paths.install_failures.iter().flatten())
    {
        let warning = format_package_install_warning(failure);
        if install_warnings.insert(warning.clone()) {
            eprintln!("{}", chalk::yellow(&format!("Warning: {warning}")));
        }
    }

    let mut resolved_paths = IndexMap::new();
    resolved_paths.insert(ConfigWriteScope::Global, global_resolved_paths);
    resolved_paths.insert(ConfigWriteScope::Project, project_resolved_paths);
    select_config(&ConfigSelectorOptions {
        resolved_paths,
        settings_manager: settings_manager.clone(),
        cwd,
        agent_dir,
        write_scope: if local {
            SettingsScope::Project
        } else {
            SettingsScope::Global
        },
        project_mode_available: settings_manager.is_project_trusted(),
    })
    .await?;

    exit(0);
}

pub async fn handle_package_command(
    args: &[String],
    runtime_options: Option<&PackageCommandRuntimeOptions>,
) -> anyhow::Result<bool> {
    let Some(options) = parse_package_command(args) else {
        return Ok(false);
    };

    if options.help {
        print_package_command_help(options.command);
        return Ok(true);
    }

    let usage = get_package_command_usage(options.command);
    if let Some(invalid_option) = &options.invalid_option {
        error(chalk::red(&format!(
            "Unknown option {invalid_option} for \"{}\".",
            options.command.as_str()
        )));
        error(chalk::dim(&format!("Use \"{APP_NAME} --help\" or \"{usage}\".")));
        set_exit_code(1);
        return Ok(true);
    }

    if let Some(missing) = &options.missing_option_value {
        error(chalk::red(&format!("Missing value for {missing}.")));
        error(chalk::dim(&format!("Usage: {usage}")));
        set_exit_code(1);
        return Ok(true);
    }

    if let Some(invalid_argument) = &options.invalid_argument {
        error(chalk::red(&format!("Unexpected argument {invalid_argument}.")));
        error(chalk::dim(&format!("Usage: {usage}")));
        set_exit_code(1);
        return Ok(true);
    }

    if let Some(conflicting) = &options.conflicting_options {
        error(chalk::red(conflicting));
        error(chalk::dim(&format!("Usage: {usage}")));
        set_exit_code(1);
        return Ok(true);
    }

    let source = options.source.clone();
    if matches!(options.command, PackageCommand::Install | PackageCommand::Remove) && source.is_none() {
        error(chalk::red(&format!("Missing {} source.", options.command.as_str())));
        error(chalk::dim(&format!("Usage: {usage}")));
        set_exit_code(1);
        return Ok(true);
    }

    if options.command == PackageCommand::Update
        && matches!(options.update_target, Some(UpdateTarget::Models {}))
    {
        if let Err(e) = refresh_model_catalogs(&get_agent_dir()).await {
            error(chalk::red(&format!("Error: {e}")));
            set_exit_code(1);
        }
        return Ok(true);
    }

    let cwd = process_cwd();
    let agent_dir = get_agent_dir();
    let writes_project_package_config =
        matches!(options.command, PackageCommand::Install | PackageCommand::Remove) && options.local;
    let CommandSettingsResult {
        settings_manager,
        project_trust_warnings,
    } = create_command_settings_manager(CreateCommandSettingsManagerOptions {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        project_trust_override: options.project_trust_override,
        use_saved_project_trust_only: options.command == PackageCommand::Update,
        extension_factories: runtime_options.and_then(|o| o.extension_factories.clone()),
    })
    .await?;
    report_project_trust_warnings(&project_trust_warnings);
    if !settings_manager.is_project_trusted() && writes_project_package_config {
        error(chalk::red(
            "Project is not trusted. Use --approve to modify local package config.",
        ));
        set_exit_code(1);
        return Ok(true);
    }
    report_settings_errors(&settings_manager, "package command");
    let self_update_npm_command = settings_manager.get_global_settings().npm_command;

    let package_manager = DefaultPackageManager::new(&PackageManagerOptions {
        cwd,
        agent_dir,
        settings_manager: settings_manager.clone(),
        builtin_extensions: None,
    });

    let progress: ProgressCallback = Arc::new(|event| {
        if event.r#type == ProgressEventType::Start {
            use std::io::Write;
            let line = chalk::dim(&format!("{}\n", event.message.unwrap_or_default()));
            #[cfg(test)]
            {
                log(line.trim_end());
                return;
            }
            #[allow(unreachable_code)]
            {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(line.as_bytes());
                let _ = stdout.flush();
            }
        }
    });
    package_manager.set_progress_callback(Some(&progress));

    let result = run_package_command(
        &options,
        source.as_deref(),
        &package_manager,
        self_update_npm_command.as_deref(),
    )
    .await;
    // Make sure settings writes reach disk before the process exits.
    let _ = settings_manager.flush().await;
    if let Err(e) = result {
        error(chalk::red(&format!("Error: {e}")));
        set_exit_code(1);
    }
    Ok(true)
}

async fn run_package_command(
    options: &PackageCommandOptions,
    source: Option<&str>,
    package_manager: &DefaultPackageManager,
    self_update_npm_command: Option<&[String]>,
) -> anyhow::Result<()> {
    match options.command {
        PackageCommand::Install => {
            let source = source.unwrap_or_default();
            package_manager
                .install_and_persist(
                    source,
                    Some(&PackageManagerInstallAndPersistOptions {
                        local: Some(options.local),
                    }),
                )
                .await?;
            log(chalk::green(&format!("Installed {source}")));
            Ok(())
        }
        PackageCommand::Remove => {
            let source = source.unwrap_or_default();
            let removed = package_manager
                .remove_and_persist(
                    source,
                    Some(&PackageManagerRemoveAndPersistOptions {
                        local: Some(options.local),
                    }),
                )
                .await?;
            if !removed {
                error(chalk::red(&format!("No matching package found for {source}")));
                set_exit_code(1);
                return Ok(());
            }
            log(chalk::green(&format!("Removed {source}")));
            Ok(())
        }
        PackageCommand::List => {
            let configured_packages = package_manager.list_configured_packages();
            if configured_packages.is_empty() {
                log(chalk::dim("No packages installed."));
                return Ok(());
            }
            let user_packages: Vec<_> = configured_packages
                .iter()
                .filter(|p| p.scope == crate::modes::interactive::components::config_selector::SettingsScope::User)
                .collect();
            let project_packages: Vec<_> = configured_packages
                .iter()
                .filter(|p| p.scope == crate::modes::interactive::components::config_selector::SettingsScope::Project)
                .collect();
            let format_package = |pkg: &crate::core::package_manager::ConfiguredPackage| {
                let display = if pkg.filtered {
                    format!("{} (filtered)", pkg.source)
                } else {
                    pkg.source.clone()
                };
                log(format!("  {display}"));
                if let Some(installed_path) = &pkg.installed_path {
                    log(chalk::dim(&format!("    {installed_path}")));
                }
            };
            if !user_packages.is_empty() {
                log(chalk::bold("User packages:"));
                for pkg in &user_packages {
                    format_package(pkg);
                }
            }
            if !project_packages.is_empty() {
                if !user_packages.is_empty() {
                    log("");
                }
                log(chalk::bold("Project packages:"));
                for pkg in &project_packages {
                    format_package(pkg);
                }
            }
            Ok(())
        }
        PackageCommand::Update => {
            let target = options
                .update_target
                .clone()
                .unwrap_or(UpdateTarget::SelfValue {});
            if options.show_extensions_skipped_note {
                log(chalk::dim(&format!(
                    "Extensions are skipped. Run {APP_NAME} update --extensions to update extensions."
                )));
            }
            if update_target_includes_extensions(&target) {
                let update_source = match &target {
                    UpdateTarget::Extensions { source } => source.clone(),
                    _ => None,
                };
                package_manager.update(update_source.as_deref()).await?;
                match update_source {
                    Some(source) => log(chalk::green(&format!("Updated {source}"))),
                    None => log(chalk::green("Updated packages")),
                }
            }
            if update_target_includes_self(&target) {
                run_self_update_command(options.force, self_update_npm_command).await?;
            }
            Ok(())
        }
    }
}

async fn run_self_update_command(force: bool, self_update_npm_command: Option<&[String]>) -> anyhow::Result<()> {
    let managed_install_root = get_active_managed_install_root()?;
    if managed_install_root.is_some() && force {
        error(chalk::red(&format!(
            "Managed {APP_NAME} installations do not support --force; rerun the installer to repair this installation."
        )));
        set_exit_code(1);
        return Ok(());
    }
    let plan = get_self_update_plan(force).await?;
    if !plan.should_run {
        return Ok(());
    }
    if let Some(managed_root) = managed_install_root {
        if let Some(note) = &plan.note {
            print_self_update_note(note);
        }
        log(chalk::dim(&format!("Updating managed {APP_NAME} installation...")));
        if let Err(e) = run_managed_self_update(&managed_root, &plan.version).await {
            error(chalk::red(&format!("Error: {e}")));
            set_exit_code(1);
            return Ok(());
        }
        log(chalk::green(&format!(
            "Updated {APP_NAME} from {VERSION} to {}",
            plan.version
        )));
        return Ok(());
    }

    let install_method = detect_install_method();
    if cfg!(windows) && install_method != InstallMethod::Npm && install_method != InstallMethod::Pnpm {
        error(chalk::red(&format!(
            "{APP_NAME} self-update on Windows is only supported for npm and pnpm installs."
        )));
        error(chalk::dim(&format!(
            "Detected install method: {}. Update {APP_NAME} manually.",
            install_method.as_str()
        )));
        set_exit_code(1);
        return Ok(());
    }
    let self_update_target = SelfUpdatePackageTarget::Spec(SelfUpdatePackageSpec {
        package_name: plan.package_name.clone(),
        install_spec: Some(plan.install_spec.clone()),
    });
    let Some(self_update_command) =
        get_self_update_command(PACKAGE_NAME, self_update_npm_command, Some(&self_update_target))
    else {
        print_self_update_unavailable(self_update_npm_command, &self_update_target);
        set_exit_code(1);
        return Ok(());
    };
    if let Some(note) = &plan.note {
        print_self_update_note(note);
    }
    if install_method == InstallMethod::Npm {
        prepare_windows_npm_self_update();
    }
    if let Err(e) = run_self_update(&self_update_command).await {
        error(chalk::red(&format!("Error: {e}")));
        if install_method == InstallMethod::Pnpm {
            print_pnpm_self_update_metadata_hint();
        }
        print_self_update_fallback(&self_update_command);
        set_exit_code(1);
        return Ok(());
    }
    log(chalk::green(&format!(
        "Updated {APP_NAME} from {VERSION} to {}",
        plan.version
    )));
    // The pi.dev installer migrates global npm installs to a managed install
    // that pins all dependencies. It does not migrate pnpm, yarn, or bun installs.
    if install_method == InstallMethod::Npm {
        let installer_command = if cfg!(windows) {
            "powershell -c \"irm https://pi.dev/install.ps1 | iex\""
        } else {
            "curl -fsSL https://pi.dev/install.sh | sh"
        };
        log("");
        log(chalk::yellow(&format!(
            "This npm installation of {APP_NAME} does not pin its dependencies."
        )));
        log(chalk::yellow("Run the installer to migrate to a managed installation that does:"));
        log("");
        log(format!("  {}", chalk::bold(installer_command)));
    }
    Ok(())
}

#[cfg(test)]
#[path = "package_manager_cli_tests.rs"]
mod tests;
