//! Port of `packages/coding-agent/src/core/resource-loader.ts`.

use crate::config::{APP_NAME, CONFIG_DIR_NAME};
use crate::core::event_bus::{EventBus, create_event_bus};
use crate::core::extensions::loader::{
    clear_extension_cache, create_extension_runtime, load_extension_from_factory,
    load_extensions_cached,
};
use crate::core::extensions::types::{
    ExtensionLoadError, ExtensionLoadWarning, ExtensionRef, ExtensionRuntime, InlineExtension,
    LoadExtensionsResult, NamedInlineExtension,
};
use crate::core::footer_data_provider::find_git_paths;
use crate::core::package_manager::{
    DefaultPackageManager, PackageInstallFailure, PackageManager, PackageManagerOptions,
    PackageManagerResolveExtensionSourcesOptions, PathMetadata, ResolvedResource,
};
use crate::core::prompt_templates::{
    LoadPromptTemplatesOptions, PromptTemplate, load_prompt_templates,
};
use crate::core::settings_manager::SettingsManager;
use crate::core::skills::{LoadSkillsOptions, Skill, load_skills};
use crate::core::source_info::{
    BUILTIN_PATH_PREFIX, SourceInfo, SourceOrigin, SourceScope, create_source_info,
    get_synthetic_path_source, is_synthetic_path,
};
use crate::core::timings::{TimingLabel, reset_timings};
use crate::modes::interactive::theme::theme::{Theme, load_theme_from_path};
use crate::utils::paths::{
    PathInputOptions, canonicalize_path, is_local_path, node_basename, node_dirname, node_join,
    node_resolve, resolve_path,
};
use crate::utils::text::strip_bom;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_tui::colors::TerminalColorMode;
use pi_tui::terminal_image::{detect_capabilities, get_terminal_color_mode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};

pub use crate::core::diagnostics::ResourceCollision;
pub use crate::core::diagnostics::ResourceDiagnostic;
use crate::core::diagnostics::{ResourceCollisionResourceType, ResourceDiagnosticType};

/// `{ path, metadata }` entry of [`ResourceExtensionPaths`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceExtensionPathsSkillPaths {
    pub path: String,
    pub metadata: PathMetadata,
}

pub type ResourceExtensionPathsPromptPaths = ResourceExtensionPathsSkillPaths;
pub type ResourceExtensionPathsThemePaths = ResourceExtensionPathsSkillPaths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceExtensionPaths {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_paths: Option<Vec<ResourceExtensionPathsSkillPaths>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_paths: Option<Vec<ResourceExtensionPathsPromptPaths>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_paths: Option<Vec<ResourceExtensionPathsThemePaths>>,
}

#[derive(Clone)]
pub struct ResourceLoaderReloadOptionsInput {
    pub extensions_result: LoadExtensionsResult,
}

/// Decides project trust after the pre-trust extension pass.
pub type ResolveProjectTrustFn = Arc<
    dyn Fn(ResourceLoaderReloadOptionsInput) -> BoxFuture<'static, anyhow::Result<bool>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct ResourceLoaderReloadOptions {
    pub resolve_project_trust: Option<ResolveProjectTrustFn>,
}

const HOST_PROVIDED_EXTENSION_PACKAGES: &[&str] = &[
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-ai",
    "@earendil-works/pi-coding-agent",
    "@earendil-works/pi-tui",
    "@mariozechner/pi-agent-core",
    "@mariozechner/pi-ai",
    "@mariozechner/pi-coding-agent",
    "@mariozechner/pi-tui",
    "@sinclair/typebox",
    "typebox",
];

fn collect_extension_package_warnings(
    extension_paths: &[String],
    metadata_by_path: &IndexMap<String, PathMetadata>,
) -> anyhow::Result<Vec<ExtensionLoadWarning>> {
    let mut warnings = Vec::new();
    let mut package_roots: Vec<String> = Vec::new();
    for extension_path in extension_paths {
        if let Some(root) = metadata_by_path
            .get(extension_path)
            .and_then(|m| m.package_root.clone())
            && !package_roots.contains(&root)
        {
            package_roots.push(root);
        }
    }
    for package_root in package_roots {
        let package_json_path = node_join(&[&package_root, "package.json"]);
        if !Path::new(&package_json_path).exists() {
            continue;
        }
        let manifest: Value =
            serde_json::from_str(&strip_bom(&std::fs::read_to_string(&package_json_path)?))?;
        let Some(dependencies) = manifest.get("dependencies").and_then(Value::as_object) else {
            continue;
        };
        let mut host_dependencies: Vec<&String> = dependencies
            .keys()
            .filter(|name| HOST_PROVIDED_EXTENSION_PACKAGES.contains(&name.as_str()))
            .collect();
        host_dependencies.sort_by(|a, b| crate::core::trust_manager::js_compare(a, b));
        if host_dependencies.is_empty() {
            continue;
        }
        warnings.push(ExtensionLoadWarning {
            path: package_json_path,
            warning: format!(
                "Host-provided extension packages must be declared in peerDependencies with a \"*\" range, not dependencies: {}. Installed copies can bypass the extension loader and create duplicate runtime modules.",
                host_dependencies
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(warnings)
}

fn describe_package_install_failure(failure: &PackageInstallFailure) -> String {
    let hint = if failure.scope == SourceScope::Temporary {
        "Check the source and try again.".to_string()
    } else {
        let local = if failure.scope == SourceScope::Project {
            " --local"
        } else {
            ""
        };
        format!("Run \"{APP_NAME} remove {}{local}\" to remove it from settings.json.", failure.source)
    };
    format!("failed to install ({}). {hint}", failure.message)
}

/// Startup warning for a package that could not be installed.
pub fn format_package_install_warning(failure: &PackageInstallFailure) -> String {
    format!("Extension package \"{}\": {}", failure.source, describe_package_install_failure(failure))
}

fn collect_package_install_warnings(
    failures: [&[PackageInstallFailure]; 2],
) -> Vec<ExtensionLoadWarning> {
    failures
        .into_iter()
        .flatten()
        .map(|failure| ExtensionLoadWarning {
            path: failure.source.clone(),
            warning: describe_package_install_failure(failure),
        })
        .collect()
}

fn merge_extension_warnings(result: &mut LoadExtensionsResult, warnings: Vec<ExtensionLoadWarning>) {
    let mut merged: IndexMap<String, ExtensionLoadWarning> = IndexMap::new();
    for warning in result.warnings.take().unwrap_or_default().into_iter().chain(warnings) {
        merged.insert(warning.path.clone(), warning);
    }
    result.warnings = Some(merged.into_values().collect());
}

pub type BuiltinExtension = NamedInlineExtension;

/// Built-in extensions supply the code of `builtin:<name>` extension paths (see `InlineExtension`).
pub fn is_builtin_extension(input: &InlineExtension) -> bool {
    matches!(input, InlineExtension::Named(named) if named.builtin == Some(true))
}

fn extension_names(extension: &ExtensionRef) -> Vec<String> {
    let ext = extension.read();
    ext.tools
        .keys()
        .map(|name| format!("tool:{name}"))
        .chain(ext.commands.keys().map(|name| format!("command:{name}")))
        .chain(ext.flags.keys().map(|name| format!("flag:{name}")))
        .collect()
}

/// Leave out replaceable extensions (see `InlineExtension`) that share a tool, command, or flag name
/// with another extension. For example, a third-party MCP extension that registers `/mcp` replaces
/// the built-in MCP extension instead of both connecting the same servers.
fn omit_replaced_extensions(
    extensions: Vec<ExtensionRef>,
    mut warnings: Option<&mut Vec<ExtensionLoadWarning>>,
) -> Vec<ExtensionRef> {
    let mut taken: IndexMap<String, ExtensionRef> = IndexMap::new();
    for extension in &extensions {
        if extension.read().replaceable == Some(true) {
            continue;
        }
        for name in extension_names(extension) {
            taken.insert(name, extension.clone());
        }
    }
    extensions
        .into_iter()
        .filter(|extension| {
            if extension.read().replaceable != Some(true) {
                return true;
            }
            let Some((name, owner)) = extension_names(extension)
                .into_iter()
                .find_map(|name| taken.get(&name).map(|owner| (name, owner.clone())))
            else {
                return true;
            };
            let path = extension.read().path.clone();
            if let Some(builtin_name) = path.strip_prefix(BUILTIN_PATH_PREFIX)
                && let Some(warnings) = warnings.as_deref_mut()
            {
                let (kind, raw_name) = name.split_once(':').unwrap_or((name.as_str(), ""));
                let registered_name = match kind {
                    "command" => format!("/{raw_name}"),
                    "flag" => format!("--{raw_name}"),
                    _ => raw_name.to_string(),
                };
                warnings.push(ExtensionLoadWarning {
                    path: path.clone(),
                    warning: format!(
                        "Extension {} registers {kind} `{registered_name}`, so built-in extension `{builtin_name}` was not loaded. To use `{builtin_name}`, run `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing extension. We recommend only having one or the other loaded at a time.",
                        owner.read().path
                    ),
                });
            }
            false
        })
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLoaderGetSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLoaderGetPromptsResult {
    pub prompts: Vec<PromptTemplate>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Clone, Default)]
pub struct ResourceLoaderGetThemesResult {
    pub themes: Vec<Arc<Theme>>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// A loaded context file (`AGENTS.md`, `CLAUDE.md`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLoaderGetAgentsFilesResultAgentsFiles {
    pub path: String,
    pub content: String,
}

pub type ContextFile = ResourceLoaderGetAgentsFilesResultAgentsFiles;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLoaderGetAgentsFilesResult {
    pub agents_files: Vec<ContextFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLoaderGetSystemPromptSourceResult {
    pub path: String,
}

pub type ResourceLoaderGetAppendSystemPromptSourcesResult = ResourceLoaderGetSystemPromptSourceResult;

#[async_trait::async_trait]
pub trait ResourceLoader: Send + Sync {
    fn get_extensions(&self) -> LoadExtensionsResult;
    fn get_skills(&self) -> ResourceLoaderGetSkillsResult;
    fn get_prompts(&self) -> ResourceLoaderGetPromptsResult;
    fn get_themes(&self) -> ResourceLoaderGetThemesResult;
    fn get_agents_files(&self) -> ResourceLoaderGetAgentsFilesResult;
    fn get_system_prompt(&self) -> Option<String>;
    fn get_system_prompt_source(&self) -> Option<ResourceLoaderGetSystemPromptSourceResult>;
    fn get_append_system_prompt(&self) -> Vec<String>;
    fn get_append_system_prompt_sources(
        &self,
    ) -> Vec<ResourceLoaderGetAppendSystemPromptSourcesResult>;
    fn extend_resources(&self, paths: &ResourceExtensionPaths);
    async fn reload(&self, options: Option<&ResourceLoaderReloadOptions>) -> anyhow::Result<()>;
}

/// Print a yellow warning to stderr (`console.error(chalk.yellow(...))`).
fn warn(message: &str) {
    eprintln!("{}", crate::utils::chalk::yellow(message));
}

fn resolve_prompt_input(input: Option<&str>, description: &str) -> Option<String> {
    let input = input.filter(|i| !i.is_empty())?;
    if Path::new(input).exists() {
        return match std::fs::read_to_string(input) {
            Ok(content) => Some(strip_bom(&content)),
            Err(error) => {
                warn(&format!(
                    "Warning: Could not read {description} file {input}: {error}"
                ));
                Some(input.to_string())
            }
        };
    }
    Some(input.to_string())
}

fn load_context_file_from_dir(dir: &str) -> Option<ContextFile> {
    const CANDIDATES: &[&str] = &[
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ];
    for filename in CANDIDATES {
        let file_path = node_join(&[dir, filename]);
        if !Path::new(&file_path).exists() {
            continue;
        }
        let result = (|| -> std::io::Result<Option<ContextFile>> {
            if !std::fs::metadata(&file_path)?.is_file() {
                return Ok(None);
            }
            Ok(Some(ContextFile {
                path: file_path.clone(),
                content: strip_bom(&std::fs::read_to_string(&file_path)?),
            }))
        })();
        match result {
            Ok(Some(file)) => return Some(file),
            Ok(None) => continue,
            Err(error) => warn(&format!("Warning: Could not read {file_path}: {error}")),
        }
    }
    None
}

/// The main repo's context file that a nested linked worktree's own copy shadows: both
/// occupy the same logical repository scope, so loading both applies that context twice. Returns
/// None when nothing is shadowed, leaving normal ancestor inheritance alone.
///
/// Returned canonicalized (realpath), because `git worktree add` writes the `.git`
/// file's `gitdir:` target in realpath form while cwd may still be symlinked
/// (macOS `/tmp` -> `/private/tmp`).
fn find_shadowed_context_file(cwd: &str) -> Option<String> {
    let git_paths = find_git_paths(cwd)?;
    let common_git_dir = canonicalize_path(&git_paths.common_git_dir);
    let worktree_root = canonicalize_path(&git_paths.repo_dir);
    let main_repo_root = node_dirname(&common_git_dir);
    // False for an ordinary repo, where the two are the same dir, and for a sibling
    // worktree (`git worktree add ../feat`), whose main repo is not an ancestor.
    if !worktree_root.starts_with(&format!("{main_repo_root}{}", std::path::MAIN_SEPARATOR)) {
        return None;
    }
    // dirname of the common git dir is the main worktree root only when that dir is
    // itself checked out from the same repo. In a bare layout (`proj/.bare` +
    // `proj/main`) it is just the directory holding `.bare`, which tracks nothing; a
    // submodule's gitdir has no `commondir`, so it lands under `.git/modules`.
    if canonicalize_path(&node_join(&[&main_repo_root, ".git"])) != common_git_dir {
        return None;
    }
    let worktree_context_file = load_context_file_from_dir(&worktree_root)?;
    Some(node_join(&[
        &main_repo_root,
        &node_basename(&worktree_context_file.path),
    ]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadProjectContextFilesOptions {
    pub cwd: String,
    pub agent_dir: String,
}

pub type LoadProjectContextFilesResult = ContextFile;

pub fn load_project_context_files(
    options: &LoadProjectContextFilesOptions,
) -> Vec<LoadProjectContextFilesResult> {
    let resolved_cwd = resolve_path(&options.cwd, None, None);
    let resolved_agent_dir = resolve_path(&options.agent_dir, None, None);

    let mut context_files: Vec<ContextFile> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    if let Some(global_context) = load_context_file_from_dir(&resolved_agent_dir) {
        seen_paths.insert(global_context.path.clone());
        context_files.push(global_context);
    }

    let mut ancestor_context_files: Vec<ContextFile> = Vec::new();
    let shadowed_context_file = find_shadowed_context_file(&resolved_cwd);
    let mut current_dir = resolved_cwd;

    loop {
        let context_file = load_context_file_from_dir(&current_dir);
        // Compare the directory's real path, not the file's: a worktree AGENTS.md that
        // symlinks to the main repo's file resolves to the shadowed path itself.
        let is_shadowed = match (&shadowed_context_file, &context_file) {
            (Some(shadowed), Some(file)) => {
                node_join(&[&canonicalize_path(&current_dir), &node_basename(&file.path)])
                    == *shadowed
            }
            _ => false,
        };
        if let Some(file) = context_file
            && !is_shadowed
            && !seen_paths.contains(&file.path)
        {
            seen_paths.insert(file.path.clone());
            ancestor_context_files.insert(0, file);
        }

        let parent_dir = node_dirname(&current_dir);
        if parent_dir == current_dir {
            break;
        }
        current_dir = parent_dir;
    }

    context_files.extend(ancestor_context_files);
    context_files
}

pub type DefaultResourceLoaderOptionsBase = ResourceLoaderGetSkillsResult;
pub type DefaultResourceLoaderOptionsResult = ResourceLoaderGetSkillsResult;
pub type DefaultResourceLoaderOptionsBase2 = ResourceLoaderGetPromptsResult;
pub type DefaultResourceLoaderOptionsResult2 = ResourceLoaderGetPromptsResult;
pub type DefaultResourceLoaderOptionsBase3 = ResourceLoaderGetThemesResult;
pub type DefaultResourceLoaderOptionsResult3 = ResourceLoaderGetThemesResult;
pub type DefaultResourceLoaderOptionsBase4AgentsFiles = ContextFile;
pub type DefaultResourceLoaderOptionsBase4 = ResourceLoaderGetAgentsFilesResult;
pub type DefaultResourceLoaderOptionsResult4AgentsFiles = ContextFile;
pub type DefaultResourceLoaderOptionsResult4 = ResourceLoaderGetAgentsFilesResult;
pub type DefaultResourceLoaderGetSkillsResult = ResourceLoaderGetSkillsResult;
pub type DefaultResourceLoaderGetPromptsResult = ResourceLoaderGetPromptsResult;
pub type DefaultResourceLoaderGetThemesResult = ResourceLoaderGetThemesResult;
pub type DefaultResourceLoaderGetAgentsFilesResultAgentsFiles = ContextFile;
pub type DefaultResourceLoaderGetAgentsFilesResult = ResourceLoaderGetAgentsFilesResult;
pub type DefaultResourceLoaderGetSystemPromptSourceResult = ResourceLoaderGetSystemPromptSourceResult;
pub type DefaultResourceLoaderGetAppendSystemPromptSourcesResult =
    ResourceLoaderGetSystemPromptSourceResult;

pub type SkillsOverrideFn =
    Arc<dyn Fn(ResourceLoaderGetSkillsResult) -> ResourceLoaderGetSkillsResult + Send + Sync>;
pub type PromptsOverrideFn =
    Arc<dyn Fn(ResourceLoaderGetPromptsResult) -> ResourceLoaderGetPromptsResult + Send + Sync>;
pub type ThemesOverrideFn =
    Arc<dyn Fn(ResourceLoaderGetThemesResult) -> ResourceLoaderGetThemesResult + Send + Sync>;
pub type AgentsFilesOverrideFn = Arc<
    dyn Fn(ResourceLoaderGetAgentsFilesResult) -> ResourceLoaderGetAgentsFilesResult + Send + Sync,
>;

#[derive(Clone, Default)]
pub struct DefaultResourceLoaderOptions {
    pub cwd: String,
    pub agent_dir: String,
    pub settings_manager: Option<Arc<SettingsManager>>,
    pub event_bus: Option<Arc<dyn EventBus>>,
    pub additional_extension_paths: Option<Vec<String>>,
    pub additional_skill_paths: Option<Vec<String>>,
    pub additional_prompt_template_paths: Option<Vec<String>>,
    pub additional_theme_paths: Option<Vec<String>>,
    pub extension_factories: Option<Vec<InlineExtension>>,
    pub no_extensions: Option<bool>,
    /// Built-in extensions not to load, by name (such as `mcp`), even when settings or `-e` enable them.
    pub disabled_builtin_extensions: Option<Vec<String>>,
    pub no_skills: Option<bool>,
    pub no_prompt_templates: Option<bool>,
    pub no_themes: Option<bool>,
    pub no_context_files: Option<bool>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub extensions_override:
        Option<Arc<dyn Fn(LoadExtensionsResult) -> LoadExtensionsResult + Send + Sync>>,
    pub skills_override: Option<SkillsOverrideFn>,
    pub prompts_override: Option<PromptsOverrideFn>,
    pub themes_override: Option<ThemesOverrideFn>,
    pub agents_files_override: Option<AgentsFilesOverrideFn>,
    pub system_prompt_override: Option<Arc<dyn Fn(Option<String>) -> Option<String> + Send + Sync>>,
    pub append_system_prompt_override:
        Option<Arc<dyn Fn(Vec<String>) -> Vec<String> + Send + Sync>>,
}

struct LoaderState {
    extensions_result: LoadExtensionsResult,
    skills: Vec<Skill>,
    skill_diagnostics: Vec<ResourceDiagnostic>,
    prompts: Vec<PromptTemplate>,
    prompt_diagnostics: Vec<ResourceDiagnostic>,
    themes: Vec<Arc<Theme>>,
    theme_diagnostics: Vec<ResourceDiagnostic>,
    agents_files: Vec<ContextFile>,
    system_prompt: Option<String>,
    system_prompt_source_path: Option<String>,
    append_system_prompt: Vec<String>,
    append_system_prompt_source_paths: Vec<String>,
    last_skill_paths: Vec<String>,
    last_prompt_paths: Vec<String>,
    last_theme_paths: Vec<String>,
    extension_skill_source_infos: IndexMap<String, SourceInfo>,
    extension_prompt_source_infos: IndexMap<String, SourceInfo>,
    extension_theme_source_infos: IndexMap<String, SourceInfo>,
    resource_metadata_by_path: IndexMap<String, PathMetadata>,
    loaded: bool,
}

/// TS: `implements ResourceLoader`
pub struct DefaultResourceLoader {
    cwd: String,
    agent_dir: String,
    settings_manager: Arc<SettingsManager>,
    event_bus: Arc<dyn EventBus>,
    package_manager: OnceLock<Arc<DefaultPackageManager>>,
    additional_extension_paths: Vec<String>,
    additional_skill_paths: Vec<String>,
    additional_prompt_template_paths: Vec<String>,
    additional_theme_paths: Vec<String>,
    extension_factories: Vec<InlineExtension>,
    builtin_extensions: IndexMap<String, BuiltinExtension>,
    no_extensions: bool,
    disabled_builtin_extensions: HashSet<String>,
    no_skills: bool,
    no_prompt_templates: bool,
    no_themes: bool,
    no_context_files: bool,
    system_prompt_source: Option<String>,
    append_system_prompt_source: Option<Vec<String>>,
    extensions_override:
        Option<Arc<dyn Fn(LoadExtensionsResult) -> LoadExtensionsResult + Send + Sync>>,
    skills_override: Option<SkillsOverrideFn>,
    prompts_override: Option<PromptsOverrideFn>,
    themes_override: Option<ThemesOverrideFn>,
    agents_files_override: Option<AgentsFilesOverrideFn>,
    system_prompt_override: Option<Arc<dyn Fn(Option<String>) -> Option<String> + Send + Sync>>,
    append_system_prompt_override: Option<Arc<dyn Fn(Vec<String>) -> Vec<String> + Send + Sync>>,
    state: Mutex<LoaderState>,
}

fn empty_extensions_result() -> LoadExtensionsResult {
    LoadExtensionsResult {
        extensions: Vec::new(),
        errors: Vec::new(),
        warnings: None,
        runtime: create_extension_runtime(),
    }
}

fn is_under_path(target: &str, root: &str) -> bool {
    let normalized_root = node_resolve(&[root]);
    if target == normalized_root {
        return true;
    }
    let sep = std::path::MAIN_SEPARATOR;
    let prefix = if normalized_root.ends_with(sep) {
        normalized_root
    } else {
        format!("{normalized_root}{sep}")
    };
    target.starts_with(&prefix)
}

fn path_is_under(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR))
}

fn cli_metadata() -> PathMetadata {
    PathMetadata {
        source: "cli".to_string(),
        scope: SourceScope::Temporary,
        origin: SourceOrigin::TopLevel,
        base_dir: None,
        package_root: None,
    }
}

impl DefaultResourceLoader {
    pub fn new(options: &DefaultResourceLoaderOptions) -> Self {
        let cwd = resolve_path(&options.cwd, None, None);
        let agent_dir = resolve_path(&options.agent_dir, None, None);
        let settings_manager = options
            .settings_manager
            .clone()
            .unwrap_or_else(|| SettingsManager::create(&cwd, Some(&agent_dir), None));
        let event_bus: Arc<dyn EventBus> = match &options.event_bus {
            Some(bus) => bus.clone(),
            None => create_event_bus(),
        };
        let factories = options.extension_factories.clone().unwrap_or_default();
        let mut extension_factories = Vec::new();
        let mut builtin_extensions = IndexMap::new();
        for input in factories {
            match input {
                InlineExtension::Named(named) if named.builtin == Some(true) => {
                    builtin_extensions.insert(named.name.clone(), named);
                }
                other => extension_factories.push(other),
            }
        }

        Self {
            cwd,
            agent_dir,
            settings_manager,
            event_bus,
            package_manager: OnceLock::new(),
            additional_extension_paths: options.additional_extension_paths.clone().unwrap_or_default(),
            additional_skill_paths: options.additional_skill_paths.clone().unwrap_or_default(),
            additional_prompt_template_paths: options
                .additional_prompt_template_paths
                .clone()
                .unwrap_or_default(),
            additional_theme_paths: options.additional_theme_paths.clone().unwrap_or_default(),
            extension_factories,
            builtin_extensions,
            no_extensions: options.no_extensions.unwrap_or(false),
            disabled_builtin_extensions: options
                .disabled_builtin_extensions
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            no_skills: options.no_skills.unwrap_or(false),
            no_prompt_templates: options.no_prompt_templates.unwrap_or(false),
            no_themes: options.no_themes.unwrap_or(false),
            no_context_files: options.no_context_files.unwrap_or(false),
            system_prompt_source: options.system_prompt.clone(),
            append_system_prompt_source: options.append_system_prompt.clone(),
            extensions_override: options.extensions_override.clone(),
            skills_override: options.skills_override.clone(),
            prompts_override: options.prompts_override.clone(),
            themes_override: options.themes_override.clone(),
            agents_files_override: options.agents_files_override.clone(),
            system_prompt_override: options.system_prompt_override.clone(),
            append_system_prompt_override: options.append_system_prompt_override.clone(),
            state: Mutex::new(LoaderState {
                extensions_result: empty_extensions_result(),
                skills: Vec::new(),
                skill_diagnostics: Vec::new(),
                prompts: Vec::new(),
                prompt_diagnostics: Vec::new(),
                themes: Vec::new(),
                theme_diagnostics: Vec::new(),
                agents_files: Vec::new(),
                system_prompt: None,
                system_prompt_source_path: None,
                append_system_prompt: Vec::new(),
                append_system_prompt_source_paths: Vec::new(),
                last_skill_paths: Vec::new(),
                last_prompt_paths: Vec::new(),
                last_theme_paths: Vec::new(),
                extension_skill_source_infos: IndexMap::new(),
                extension_prompt_source_infos: IndexMap::new(),
                extension_theme_source_infos: IndexMap::new(),
                resource_metadata_by_path: IndexMap::new(),
                loaded: false,
            }),
        }
    }

    /// The settings manager this loader reads (and reloads).
    pub fn settings_manager(&self) -> Arc<SettingsManager> {
        self.settings_manager.clone()
    }

    fn package_manager(&self) -> Arc<DefaultPackageManager> {
        self.package_manager
            .get_or_init(|| {
                Arc::new(DefaultPackageManager::new(&PackageManagerOptions {
                    cwd: self.cwd.clone(),
                    agent_dir: self.agent_dir.clone(),
                    settings_manager: self.settings_manager.clone(),
                    builtin_extensions: Some(self.builtin_extensions.keys().cloned().collect()),
                }))
            })
            .clone()
    }

    async fn resolve_cli_extension_sources(&self) -> anyhow::Result<crate::core::package_manager::ResolvedPaths> {
        self.package_manager()
            .resolve_extension_sources(
                &self.additional_extension_paths,
                Some(&PackageManagerResolveExtensionSourcesOptions {
                    local: None,
                    temporary: Some(true),
                }),
            )
            .await
    }

    pub async fn load_project_trust_extensions(&self) -> anyhow::Result<LoadExtensionsResult> {
        // Force untrusted project settings for the bootstrap pass. This keeps project-local
        // extensions/packages out while still loading user/global and temporary CLI extensions.
        self.settings_manager.set_project_trusted(false);
        self.settings_manager.reload().await?;
        self.load_current_extension_set(true).await
    }

    async fn reload_impl(&self, options: Option<&ResourceLoaderReloadOptions>) -> anyhow::Result<()> {
        reset_timings(Some(&TimingLabel::Extensions));

        if self.state.lock().loaded {
            clear_extension_cache();
        }

        let mut pre_trust_extensions: Option<LoadExtensionsResult> = None;
        if let Some(resolve_project_trust) = options.and_then(|o| o.resolve_project_trust.clone()) {
            let extensions = self.load_project_trust_extensions().await?;
            pre_trust_extensions = Some(extensions.clone());
            let project_trusted = resolve_project_trust(ResourceLoaderReloadOptionsInput {
                extensions_result: extensions,
            })
            .await?;
            self.settings_manager.set_project_trusted(project_trusted);
        }

        // reload() preserves SettingsManager.projectTrusted and reloads settings for that trust state.
        self.settings_manager.reload().await?;
        let resolved_paths = self.package_manager().resolve(None).await?;
        let cli_extension_paths = self.resolve_cli_extension_sources().await?;

        // Kept on the instance so post-reload passes (extendResources) can still resolve package metadata.
        let mut metadata_by_path: IndexMap<String, PathMetadata> = IndexMap::new();
        {
            let mut state = self.state.lock();
            state.extension_skill_source_infos = IndexMap::new();
            state.extension_prompt_source_infos = IndexMap::new();
            state.extension_theme_source_infos = IndexMap::new();
        }

        // Helper to extract enabled paths and store metadata
        fn get_enabled_resources(
            resources: &[ResolvedResource],
            metadata_by_path: &mut IndexMap<String, PathMetadata>,
        ) -> Vec<ResolvedResource> {
            for r in resources {
                if !metadata_by_path.contains_key(&r.path) {
                    metadata_by_path.insert(r.path.clone(), r.metadata.clone());
                }
            }
            resources.iter().filter(|r| r.enabled).cloned().collect()
        }
        fn get_enabled_paths(
            resources: &[ResolvedResource],
            metadata_by_path: &mut IndexMap<String, PathMetadata>,
        ) -> Vec<String> {
            get_enabled_resources(resources, metadata_by_path)
                .into_iter()
                .map(|r| r.path)
                .collect()
        }

        let enabled_extensions = get_enabled_paths(&resolved_paths.extensions, &mut metadata_by_path);
        let enabled_skill_resources =
            get_enabled_resources(&resolved_paths.skills, &mut metadata_by_path);
        let enabled_prompts = get_enabled_paths(&resolved_paths.prompts, &mut metadata_by_path);
        let enabled_themes = get_enabled_paths(&resolved_paths.themes, &mut metadata_by_path);

        let enabled_skills: Vec<String> = enabled_skill_resources
            .iter()
            .map(|resource| map_skill_path(resource, &mut metadata_by_path))
            .collect();

        // Add CLI paths metadata
        for r in cli_extension_paths
            .extensions
            .iter()
            .chain(cli_extension_paths.skills.iter())
        {
            if !metadata_by_path.contains_key(&r.path) {
                metadata_by_path.insert(r.path.clone(), cli_metadata());
            }
        }

        let cli_enabled_extensions =
            get_enabled_paths(&cli_extension_paths.extensions, &mut metadata_by_path);
        let cli_enabled_skills = get_enabled_paths(&cli_extension_paths.skills, &mut metadata_by_path);
        let cli_enabled_prompts =
            get_enabled_paths(&cli_extension_paths.prompts, &mut metadata_by_path);
        let cli_enabled_themes = get_enabled_paths(&cli_extension_paths.themes, &mut metadata_by_path);

        let extension_paths: Vec<String> = if self.no_extensions {
            cli_enabled_extensions
        } else {
            self.merge_paths(&cli_enabled_extensions, &enabled_extensions)
        }
        .into_iter()
        .filter(|path| match path.strip_prefix(BUILTIN_PATH_PREFIX) {
            Some(name) => !self.disabled_builtin_extensions.contains(name),
            None => true,
        })
        .collect();

        let package_warnings = collect_extension_package_warnings(&extension_paths, &metadata_by_path)?;
        let mut extensions_result = self
            .load_final_extension_set(&extension_paths, pre_trust_extensions)
            .await?;
        let install_warnings = collect_package_install_warnings([
            resolved_paths.install_failures.as_deref().unwrap_or_default(),
            cli_extension_paths.install_failures.as_deref().unwrap_or_default(),
        ]);
        merge_extension_warnings(
            &mut extensions_result,
            install_warnings.into_iter().chain(package_warnings).collect(),
        );
        for p in &self.additional_extension_paths {
            if is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                if !Path::new(&resolved).exists() {
                    extensions_result.errors.push(ExtensionLoadError {
                        error: format!("Extension path does not exist: {resolved}"),
                        path: resolved,
                    });
                }
            }
        }
        let extensions_result = match &self.extensions_override {
            Some(override_fn) => override_fn(extensions_result),
            None => extensions_result,
        };
        self.apply_extension_source_info(&extensions_result.extensions, &metadata_by_path);

        {
            let mut state = self.state.lock();
            state.extensions_result = extensions_result;
            state.resource_metadata_by_path = metadata_by_path.clone();
        }

        let skill_paths = if self.no_skills {
            self.merge_paths(&cli_enabled_skills, &self.additional_skill_paths)
        } else {
            let primary: Vec<String> = cli_enabled_skills
                .iter()
                .chain(enabled_skills.iter())
                .cloned()
                .collect();
            self.merge_paths(&primary, &self.additional_skill_paths)
        };
        self.state.lock().last_skill_paths = skill_paths.clone();
        self.update_skills_from_paths(&skill_paths, Some(&metadata_by_path));
        for p in &self.additional_skill_paths {
            if is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                let mut state = self.state.lock();
                if !Path::new(&resolved).exists()
                    && !state
                        .skill_diagnostics
                        .iter()
                        .any(|d| d.path.as_deref() == Some(resolved.as_str()))
                {
                    state.skill_diagnostics.push(ResourceDiagnostic {
                        r#type: ResourceDiagnosticType::Error,
                        message: "Skill path does not exist".to_string(),
                        path: Some(resolved),
                        collision: None,
                    });
                }
            }
        }

        let prompt_paths = if self.no_prompt_templates {
            self.merge_paths(&cli_enabled_prompts, &self.additional_prompt_template_paths)
        } else {
            let primary: Vec<String> = cli_enabled_prompts
                .iter()
                .chain(enabled_prompts.iter())
                .cloned()
                .collect();
            self.merge_paths(&primary, &self.additional_prompt_template_paths)
        };
        self.state.lock().last_prompt_paths = prompt_paths.clone();
        self.update_prompts_from_paths(&prompt_paths, Some(&metadata_by_path));
        for p in &self.additional_prompt_template_paths {
            if is_local_path(p) {
                let resolved = self.resolve_resource_path(p);
                let mut state = self.state.lock();
                if !Path::new(&resolved).exists()
                    && !state
                        .prompt_diagnostics
                        .iter()
                        .any(|d| d.path.as_deref() == Some(resolved.as_str()))
                {
                    state.prompt_diagnostics.push(ResourceDiagnostic {
                        r#type: ResourceDiagnosticType::Error,
                        message: "Prompt template path does not exist".to_string(),
                        path: Some(resolved),
                        collision: None,
                    });
                }
            }
        }

        let theme_paths = if self.no_themes {
            self.merge_paths(&cli_enabled_themes, &self.additional_theme_paths)
        } else {
            let primary: Vec<String> = cli_enabled_themes
                .iter()
                .chain(enabled_themes.iter())
                .cloned()
                .collect();
            self.merge_paths(&primary, &self.additional_theme_paths)
        };
        self.state.lock().last_theme_paths = theme_paths.clone();
        self.update_themes_from_paths(&theme_paths, Some(&metadata_by_path));
        for p in &self.additional_theme_paths {
            let resolved = self.resolve_resource_path(p);
            let mut state = self.state.lock();
            if !Path::new(&resolved).exists()
                && !state
                    .theme_diagnostics
                    .iter()
                    .any(|d| d.path.as_deref() == Some(resolved.as_str()))
            {
                state.theme_diagnostics.push(ResourceDiagnostic {
                    r#type: ResourceDiagnosticType::Error,
                    message: "Theme path does not exist".to_string(),
                    path: Some(resolved),
                    collision: None,
                });
            }
        }

        let agents_files = ResourceLoaderGetAgentsFilesResult {
            agents_files: if self.no_context_files {
                Vec::new()
            } else {
                load_project_context_files(&LoadProjectContextFilesOptions {
                    cwd: self.cwd.clone(),
                    agent_dir: self.agent_dir.clone(),
                })
            },
        };
        let resolved_agents_files = match &self.agents_files_override {
            Some(override_fn) => override_fn(agents_files),
            None => agents_files,
        };

        let system_prompt_source = self
            .system_prompt_source
            .clone()
            .or_else(|| self.discover_system_prompt_file());
        let base_system_prompt =
            resolve_prompt_input(system_prompt_source.as_deref(), "system prompt");
        let system_prompt = match &self.system_prompt_override {
            Some(override_fn) => override_fn(base_system_prompt),
            None => base_system_prompt,
        };
        let system_prompt_source_path = system_prompt_source
            .as_deref()
            .filter(|s| !s.is_empty() && Path::new(s).exists())
            .map(|s| resolve_path(s, None, None));

        let append_sources: Vec<String> = match &self.append_system_prompt_source {
            Some(sources) => sources.clone(),
            None => self
                .discover_append_system_prompt_file()
                .into_iter()
                .collect(),
        };
        let base_append: Vec<String> = append_sources
            .iter()
            .filter_map(|s| resolve_prompt_input(Some(s), "append system prompt"))
            .collect();
        let append_system_prompt = match &self.append_system_prompt_override {
            Some(override_fn) => override_fn(base_append),
            None => base_append,
        };
        let append_system_prompt_source_paths: Vec<String> = append_sources
            .iter()
            .filter(|source| Path::new(source).exists())
            .map(|source| resolve_path(source, None, None))
            .collect();

        let mut state = self.state.lock();
        state.agents_files = resolved_agents_files.agents_files;
        state.system_prompt = system_prompt;
        state.system_prompt_source_path = system_prompt_source_path;
        state.append_system_prompt = append_system_prompt;
        state.append_system_prompt_source_paths = append_system_prompt_source_paths;
        state.loaded = true;
        Ok(())
    }

    async fn load_current_extension_set(
        &self,
        include_inline_factories: bool,
    ) -> anyhow::Result<LoadExtensionsResult> {
        let resolved_paths = self.package_manager().resolve(None).await?;
        let cli_extension_paths = self.resolve_cli_extension_sources().await?;
        let enabled_extensions: Vec<String> = resolved_paths
            .extensions
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.clone())
            .collect();
        let cli_enabled_extensions: Vec<String> = cli_extension_paths
            .extensions
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.path.clone())
            .collect();
        // Built-in extensions wait for the final pass: project settings can disable them, and a loaded
        // extension cannot be unloaded.
        let extension_paths: Vec<String> = if self.no_extensions {
            cli_enabled_extensions
        } else {
            self.merge_paths(&cli_enabled_extensions, &enabled_extensions)
        }
        .into_iter()
        .filter(|path| !path.starts_with(BUILTIN_PATH_PREFIX))
        .collect();
        let mut metadata_by_path: IndexMap<String, PathMetadata> = IndexMap::new();
        for resource in resolved_paths
            .extensions
            .iter()
            .chain(cli_extension_paths.extensions.iter())
        {
            metadata_by_path.insert(resource.path.clone(), resource.metadata.clone());
        }
        let package_warnings = collect_extension_package_warnings(&extension_paths, &metadata_by_path)?;
        let mut extensions_result = load_extensions_cached(
            &extension_paths,
            &self.cwd,
            Some(self.event_bus.clone()),
            None,
        )
        .await?;
        let install_warnings = collect_package_install_warnings([
            resolved_paths.install_failures.as_deref().unwrap_or_default(),
            cli_extension_paths.install_failures.as_deref().unwrap_or_default(),
        ]);
        merge_extension_warnings(
            &mut extensions_result,
            install_warnings.into_iter().chain(package_warnings).collect(),
        );
        if !include_inline_factories {
            return Ok(extensions_result);
        }

        let (inline_extensions, inline_errors) = self
            .load_extension_factories(extensions_result.runtime.clone())
            .await;
        extensions_result.extensions.extend(inline_extensions);
        extensions_result.errors.extend(inline_errors);
        let mut replacement_warnings = Vec::new();
        extensions_result.extensions = omit_replaced_extensions(
            std::mem::take(&mut extensions_result.extensions),
            Some(&mut replacement_warnings),
        );
        merge_extension_warnings(&mut extensions_result, replacement_warnings);
        Ok(extensions_result)
    }

    fn resolve_extension_load_path(&self, path: &str) -> String {
        if is_synthetic_path(path) {
            path.to_string()
        } else {
            resolve_path(
                path,
                Some(&self.cwd),
                Some(&PathInputOptions {
                    normalize_unicode_spaces: Some(true),
                    ..Default::default()
                }),
            )
        }
    }

    /// Load extension paths: files from disk and `builtin:<name>` paths from the built-in extensions.
    async fn load_extension_paths(
        &self,
        paths: &[String],
        runtime: Option<Arc<ExtensionRuntime>>,
    ) -> anyhow::Result<LoadExtensionsResult> {
        let is_builtin_path = |path: &String| path.starts_with(BUILTIN_PATH_PREFIX);
        let file_paths: Vec<String> = paths.iter().filter(|p| !is_builtin_path(p)).cloned().collect();
        let mut result =
            load_extensions_cached(&file_paths, &self.cwd, Some(self.event_bus.clone()), runtime)
                .await?;
        for path in paths.iter().filter(|p| is_builtin_path(p)) {
            let Some(builtin) = self
                .builtin_extensions
                .get(&path[BUILTIN_PATH_PREFIX.len()..])
            else {
                result.errors.push(ExtensionLoadError {
                    path: path.clone(),
                    error: format!("Unknown built-in extension: {path}"),
                });
                continue;
            };
            match load_extension_from_factory(
                &builtin.factory,
                &self.cwd,
                self.event_bus.clone(),
                result.runtime.clone(),
                Some(path),
            )
            .await
            {
                Ok(extension) => {
                    {
                        let mut ext = extension.write();
                        ext.hidden = Some(true);
                        ext.replaceable = Some(builtin.replaceable == Some(true));
                    }
                    result.extensions.push(extension);
                }
                Err(error) => result.errors.push(ExtensionLoadError {
                    path: path.clone(),
                    error: error.to_string(),
                }),
            }
        }
        Ok(result)
    }

    async fn load_final_extension_set(
        &self,
        extension_paths: &[String],
        pre_trust_extensions: Option<LoadExtensionsResult>,
    ) -> anyhow::Result<LoadExtensionsResult> {
        // Without a pre-trust pass nothing is preloaded, and inline extensions load here.
        let preloaded: Vec<ExtensionRef> = pre_trust_extensions
            .as_ref()
            .map(|r| r.extensions.clone())
            .unwrap_or_default();
        let mut preloaded_by_path: IndexMap<String, ExtensionRef> = IndexMap::new();
        for extension in &preloaded {
            let ext = extension.read();
            if !ext.path.starts_with("<inline:") {
                preloaded_by_path.insert(ext.resolved_path.clone(), extension.clone());
            }
        }
        let failed_preload_paths: HashSet<String> = pre_trust_extensions
            .as_ref()
            .map(|r| {
                r.errors
                    .iter()
                    .map(|error| self.resolve_extension_load_path(&error.path))
                    .collect()
            })
            .unwrap_or_default();
        let remaining_paths: Vec<String> = extension_paths
            .iter()
            .filter(|path| {
                let resolved_path = self.resolve_extension_load_path(path);
                !preloaded_by_path.contains_key(&resolved_path)
                    && !failed_preload_paths.contains(&resolved_path)
            })
            .cloned()
            .collect();
        let remaining_extensions = self
            .load_extension_paths(
                &remaining_paths,
                pre_trust_extensions.as_ref().map(|r| r.runtime.clone()),
            )
            .await?;
        let mut loaded_by_path = preloaded_by_path.clone();
        for extension in &remaining_extensions.extensions {
            let resolved_path = extension.read().resolved_path.clone();
            loaded_by_path.insert(resolved_path, extension.clone());
        }

        let (inline_extensions, inline_errors) = match &pre_trust_extensions {
            Some(_) => (
                preloaded
                    .iter()
                    .filter(|extension| extension.read().path.starts_with("<inline:"))
                    .cloned()
                    .collect::<Vec<_>>(),
                Vec::new(),
            ),
            None => {
                self.load_extension_factories(remaining_extensions.runtime.clone())
                    .await
            }
        };
        let mut ordered_extensions: Vec<ExtensionRef> = extension_paths
            .iter()
            .filter_map(|path| {
                loaded_by_path
                    .get(&self.resolve_extension_load_path(path))
                    .cloned()
            })
            .collect();
        ordered_extensions.extend(inline_extensions);

        let mut replacement_warnings = Vec::new();
        let mut errors: Vec<ExtensionLoadError> = pre_trust_extensions
            .as_ref()
            .map(|r| r.errors.clone())
            .unwrap_or_default();
        errors.extend(remaining_extensions.errors.clone());
        errors.extend(inline_errors);
        let mut warnings: Vec<ExtensionLoadWarning> = pre_trust_extensions
            .as_ref()
            .and_then(|r| r.warnings.clone())
            .unwrap_or_default();
        warnings.extend(remaining_extensions.warnings.clone().unwrap_or_default());
        let mut extensions_result = LoadExtensionsResult {
            extensions: omit_replaced_extensions(ordered_extensions, Some(&mut replacement_warnings)),
            errors,
            warnings: Some(warnings),
            runtime: remaining_extensions.runtime.clone(),
        };
        merge_extension_warnings(&mut extensions_result, replacement_warnings);
        self.add_extension_conflict_diagnostics(&mut extensions_result);
        Ok(extensions_result)
    }

    fn add_extension_conflict_diagnostics(&self, extensions_result: &mut LoadExtensionsResult) {
        // Detect extension conflicts (tools, commands, flags with same names from different extensions)
        // Keep all extensions loaded. Conflicts are reported as diagnostics, and precedence is handled by load order.
        let conflicts = detect_extension_conflicts(&extensions_result.extensions);
        for (path, message) in conflicts {
            extensions_result.errors.push(ExtensionLoadError {
                path,
                error: message,
            });
        }
    }

    fn normalize_extension_paths(
        &self,
        entries: &[ResourceExtensionPathsSkillPaths],
    ) -> Vec<ResourceExtensionPathsSkillPaths> {
        entries
            .iter()
            .map(|entry| {
                let mut metadata = entry.metadata.clone();
                if let Some(base_dir) = &entry.metadata.base_dir
                    && !base_dir.is_empty()
                {
                    metadata.base_dir = Some(self.resolve_resource_path(base_dir));
                }
                ResourceExtensionPathsSkillPaths {
                    path: self.resolve_resource_path(&entry.path),
                    metadata,
                }
            })
            .collect()
    }

    fn update_skills_from_paths(
        &self,
        skill_paths: &[String],
        metadata_by_path: Option<&IndexMap<String, PathMetadata>>,
    ) {
        let skills_result = if self.no_skills && skill_paths.is_empty() {
            ResourceLoaderGetSkillsResult::default()
        } else {
            let loaded = load_skills(&LoadSkillsOptions {
                cwd: self.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                skill_paths: skill_paths.to_vec(),
                include_defaults: false,
            });
            ResourceLoaderGetSkillsResult {
                skills: loaded.skills,
                diagnostics: loaded.diagnostics,
            }
        };
        let resolved = match &self.skills_override {
            Some(override_fn) => override_fn(skills_result),
            None => skills_result,
        };
        let extra = self.state.lock().extension_skill_source_infos.clone();
        let skills: Vec<Skill> = resolved
            .skills
            .into_iter()
            .map(|mut skill| {
                skill.source_info = self
                    .find_source_info_for_path(&skill.file_path, Some(&extra), metadata_by_path)
                    .unwrap_or(skill.source_info);
                skill
            })
            .collect();
        let mut state = self.state.lock();
        state.skills = skills;
        state.skill_diagnostics = resolved.diagnostics;
    }

    fn update_prompts_from_paths(
        &self,
        prompt_paths: &[String],
        metadata_by_path: Option<&IndexMap<String, PathMetadata>>,
    ) {
        let prompts_result = if self.no_prompt_templates && prompt_paths.is_empty() {
            ResourceLoaderGetPromptsResult::default()
        } else {
            let loaded = load_prompt_templates(&LoadPromptTemplatesOptions {
                cwd: self.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                prompt_paths: prompt_paths.to_vec(),
                include_defaults: false,
            });
            let (prompts, dedupe_diagnostics) = dedupe_prompts(loaded.templates);
            let mut diagnostics = loaded.diagnostics;
            diagnostics.extend(dedupe_diagnostics);
            ResourceLoaderGetPromptsResult {
                prompts,
                diagnostics,
            }
        };
        let resolved = match &self.prompts_override {
            Some(override_fn) => override_fn(prompts_result),
            None => prompts_result,
        };
        let extra = self.state.lock().extension_prompt_source_infos.clone();
        let prompts: Vec<PromptTemplate> = resolved
            .prompts
            .into_iter()
            .map(|mut prompt| {
                prompt.source_info = self
                    .find_source_info_for_path(&prompt.file_path, Some(&extra), metadata_by_path)
                    .unwrap_or(prompt.source_info);
                prompt
            })
            .collect();
        let mut state = self.state.lock();
        state.prompts = prompts;
        state.prompt_diagnostics = resolved.diagnostics;
    }

    fn update_themes_from_paths(
        &self,
        theme_paths: &[String],
        metadata_by_path: Option<&IndexMap<String, PathMetadata>>,
    ) {
        let themes_result = if self.no_themes && theme_paths.is_empty() {
            ResourceLoaderGetThemesResult::default()
        } else {
            // Theme construction only needs trueColor, so skip the unrelated tmux hyperlink probe.
            let mut capabilities = detect_capabilities(Some(&|| false));
            let overrides = self.settings_manager.get_terminal_capability_overrides();
            if let Some(images) = overrides.images {
                capabilities.images = images;
            }
            if let Some(true_color) = overrides.true_color {
                capabilities.true_color = true_color;
            }
            if let Some(hyperlinks) = overrides.hyperlinks {
                capabilities.hyperlinks = hyperlinks;
            }
            let color_mode = get_terminal_color_mode(Some(capabilities));
            let (themes, mut diagnostics) = self.load_themes(theme_paths, false, color_mode);
            let (themes, dedupe_diagnostics) = dedupe_themes(themes);
            diagnostics.extend(dedupe_diagnostics);
            ResourceLoaderGetThemesResult {
                themes,
                diagnostics,
            }
        };
        let resolved = match &self.themes_override {
            Some(override_fn) => override_fn(themes_result),
            None => themes_result,
        };
        let extra = self.state.lock().extension_theme_source_infos.clone();
        for theme in &resolved.themes {
            if let Some(source_path) = theme.source_path() {
                let source_info = self
                    .find_source_info_for_path(&source_path, Some(&extra), metadata_by_path)
                    .or_else(|| theme.source_info())
                    .unwrap_or_else(|| self.get_default_source_info_for_path(&source_path));
                theme.set_source_info(Some(source_info));
            }
        }
        let mut state = self.state.lock();
        state.themes = resolved.themes;
        state.theme_diagnostics = resolved.diagnostics;
    }

    fn apply_extension_source_info(
        &self,
        extensions: &[ExtensionRef],
        metadata_by_path: &IndexMap<String, PathMetadata>,
    ) {
        for extension in extensions {
            let mut ext = extension.write();
            let source_info = self
                .find_source_info_for_path(&ext.path, None, Some(metadata_by_path))
                .unwrap_or_else(|| self.get_default_source_info_for_path(&ext.path));
            ext.source_info = source_info.clone();
            for command in ext.commands.values_mut() {
                command.source_info = source_info.clone();
            }
            for tool in ext.tools.values_mut() {
                tool.source_info = source_info.clone();
            }
        }
    }

    fn find_source_info_for_path(
        &self,
        resource_path: &str,
        extra_source_infos: Option<&IndexMap<String, SourceInfo>>,
        metadata_by_path: Option<&IndexMap<String, PathMetadata>>,
    ) -> Option<SourceInfo> {
        if resource_path.is_empty() {
            return None;
        }

        if is_synthetic_path(resource_path) {
            return Some(self.get_default_source_info_for_path(resource_path));
        }

        let normalized_resource_path = node_resolve(&[resource_path]);
        if let Some(extra) = extra_source_infos {
            for (source_path, source_info) in extra {
                if path_is_under(&normalized_resource_path, &node_resolve(&[source_path])) {
                    let mut info = source_info.clone();
                    info.path = resource_path.to_string();
                    return Some(info);
                }
            }
        }

        if let Some(metadata_by_path) = metadata_by_path {
            if let Some(exact) = metadata_by_path
                .get(&normalized_resource_path)
                .or_else(|| metadata_by_path.get(resource_path))
            {
                return Some(create_source_info(resource_path, exact));
            }
            for (source_path, metadata) in metadata_by_path {
                if path_is_under(&normalized_resource_path, &node_resolve(&[source_path])) {
                    return Some(create_source_info(resource_path, metadata));
                }
            }
        }

        None
    }

    fn get_default_source_info_for_path(&self, file_path: &str) -> SourceInfo {
        if let Some(synthetic_source) = get_synthetic_path_source(file_path) {
            return SourceInfo {
                path: file_path.to_string(),
                source: synthetic_source,
                scope: SourceScope::Temporary,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            };
        }

        let normalized_path = node_resolve(&[file_path]);
        let agent_roots = [
            node_join(&[&self.agent_dir, "skills"]),
            node_join(&[&self.agent_dir, "prompts"]),
            node_join(&[&self.agent_dir, "themes"]),
            node_join(&[&self.agent_dir, "extensions"]),
        ];
        let project_roots = [
            node_join(&[&self.cwd, CONFIG_DIR_NAME, "skills"]),
            node_join(&[&self.cwd, CONFIG_DIR_NAME, "prompts"]),
            node_join(&[&self.cwd, CONFIG_DIR_NAME, "themes"]),
            node_join(&[&self.cwd, CONFIG_DIR_NAME, "extensions"]),
        ];

        for root in agent_roots {
            if is_under_path(&normalized_path, &root) {
                return SourceInfo {
                    path: file_path.to_string(),
                    source: "local".to_string(),
                    scope: SourceScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(root),
                };
            }
        }

        for root in project_roots {
            if is_under_path(&normalized_path, &root) {
                return SourceInfo {
                    path: file_path.to_string(),
                    source: "local".to_string(),
                    scope: SourceScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(root),
                };
            }
        }

        let base_dir = if std::fs::metadata(&normalized_path).is_ok_and(|m| m.is_dir()) {
            normalized_path.clone()
        } else {
            node_resolve(&[&normalized_path, ".."])
        };
        SourceInfo {
            path: file_path.to_string(),
            source: "local".to_string(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(base_dir),
        }
    }

    fn merge_paths(&self, primary: &[String], additional: &[String]) -> Vec<String> {
        let mut merged = Vec::new();
        let mut seen = HashSet::new();
        for p in primary.iter().chain(additional.iter()) {
            let resolved = self.resolve_resource_path(p);
            let canonical_path = canonicalize_path(&resolved);
            if !seen.insert(canonical_path) {
                continue;
            }
            merged.push(resolved);
        }
        merged
    }

    fn resolve_resource_path(&self, p: &str) -> String {
        if is_synthetic_path(p) {
            p.to_string()
        } else {
            resolve_path(
                p,
                Some(&self.cwd),
                Some(&PathInputOptions {
                    trim: Some(true),
                    ..Default::default()
                }),
            )
        }
    }

    fn load_themes(
        &self,
        paths: &[String],
        include_defaults: bool,
        color_mode: TerminalColorMode,
    ) -> (Vec<Arc<Theme>>, Vec<ResourceDiagnostic>) {
        let mut themes = Vec::new();
        let mut diagnostics = Vec::new();
        if include_defaults {
            for dir in [
                node_join(&[&self.agent_dir, "themes"]),
                node_join(&[&self.cwd, CONFIG_DIR_NAME, "themes"]),
            ] {
                load_themes_from_dir(&dir, &mut themes, &mut diagnostics, color_mode);
            }
        }

        for p in paths {
            let resolved = self.resolve_resource_path(p);
            if !Path::new(&resolved).exists() {
                diagnostics.push(theme_warning("theme path does not exist", &resolved));
                continue;
            }
            match std::fs::metadata(&resolved) {
                Ok(stats) if stats.is_dir() => {
                    load_themes_from_dir(&resolved, &mut themes, &mut diagnostics, color_mode)
                }
                Ok(stats) if stats.is_file() && resolved.ends_with(".json") => {
                    load_theme_from_file(&resolved, &mut themes, &mut diagnostics, color_mode)
                }
                Ok(_) => diagnostics.push(theme_warning("theme path is not a json file", &resolved)),
                Err(error) => diagnostics.push(theme_warning(&error.to_string(), &resolved)),
            }
        }

        (themes, diagnostics)
    }

    async fn load_extension_factories(
        &self,
        runtime: Arc<ExtensionRuntime>,
    ) -> (Vec<ExtensionRef>, Vec<ExtensionLoadError>) {
        let mut extensions = Vec::new();
        let mut errors = Vec::new();

        for (index, input) in self.extension_factories.iter().enumerate() {
            let (factory, extension_path, named) = match input {
                InlineExtension::Factory(factory) => {
                    (factory.clone(), format!("<inline:{}>", index + 1), None)
                }
                InlineExtension::Named(named) => (
                    named.factory.clone(),
                    format!("<inline:{}>", named.name),
                    Some(named),
                ),
            };
            match load_extension_from_factory(
                &factory,
                &self.cwd,
                self.event_bus.clone(),
                runtime.clone(),
                Some(&extension_path),
            )
            .await
            {
                Ok(extension) => {
                    {
                        let mut ext = extension.write();
                        ext.hidden = Some(named.is_some_and(|n| n.hidden == Some(true)));
                        ext.replaceable = Some(named.is_some_and(|n| n.replaceable == Some(true)));
                    }
                    extensions.push(extension);
                }
                Err(error) => errors.push(ExtensionLoadError {
                    path: extension_path,
                    error: error.to_string(),
                }),
            }
        }

        (extensions, errors)
    }

    fn discover_system_prompt_file(&self) -> Option<String> {
        let project_path = node_join(&[&self.cwd, CONFIG_DIR_NAME, "SYSTEM.md"]);
        if self.settings_manager.is_project_trusted() && Path::new(&project_path).exists() {
            return Some(project_path);
        }
        let global_path = node_join(&[&self.agent_dir, "SYSTEM.md"]);
        Path::new(&global_path).exists().then_some(global_path)
    }

    fn discover_append_system_prompt_file(&self) -> Option<String> {
        let project_path = node_join(&[&self.cwd, CONFIG_DIR_NAME, "APPEND_SYSTEM.md"]);
        if self.settings_manager.is_project_trusted() && Path::new(&project_path).exists() {
            return Some(project_path);
        }
        let global_path = node_join(&[&self.agent_dir, "APPEND_SYSTEM.md"]);
        Path::new(&global_path).exists().then_some(global_path)
    }
}

fn map_skill_path(
    resource: &ResolvedResource,
    metadata_by_path: &mut IndexMap<String, PathMetadata>,
) -> String {
    if resource.metadata.source != "auto" && resource.metadata.origin != SourceOrigin::Package {
        return resource.path.clone();
    }
    match std::fs::metadata(&resource.path) {
        Ok(stats) if stats.is_dir() => {}
        _ => return resource.path.clone(),
    }
    let skill_file = node_join(&[&resource.path, "SKILL.md"]);
    if Path::new(&skill_file).exists() {
        if !metadata_by_path.contains_key(&skill_file) {
            metadata_by_path.insert(skill_file.clone(), resource.metadata.clone());
        }
        return skill_file;
    }
    resource.path.clone()
}

fn theme_warning(message: &str, path: &str) -> ResourceDiagnostic {
    ResourceDiagnostic {
        r#type: ResourceDiagnosticType::Warning,
        message: message.to_string(),
        path: Some(path.to_string()),
        collision: None,
    }
}

fn load_themes_from_dir(
    dir: &str,
    themes: &mut Vec<Arc<Theme>>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    color_mode: TerminalColorMode,
) {
    if !Path::new(dir).exists() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            entries
        }
        Err(error) => {
            diagnostics.push(theme_warning(&error.to_string(), dir));
            return;
        }
    };
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_path = node_join(&[dir, &name]);
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let mut is_file = file_type.is_file();
        if file_type.is_symlink() {
            match std::fs::metadata(&full_path) {
                Ok(meta) => is_file = meta.is_file(),
                Err(_) => continue,
            }
        }
        if !is_file || !name.ends_with(".json") {
            continue;
        }
        load_theme_from_file(&full_path, themes, diagnostics, color_mode);
    }
}

fn load_theme_from_file(
    file_path: &str,
    themes: &mut Vec<Arc<Theme>>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    color_mode: TerminalColorMode,
) {
    match load_theme_from_path(file_path, Some(color_mode)) {
        Ok(theme) => themes.push(theme),
        Err(error) => diagnostics.push(theme_warning(&error.to_string(), file_path)),
    }
}

fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut seen: IndexMap<String, PromptTemplate> = IndexMap::new();
    let mut diagnostics = Vec::new();
    for prompt in prompts {
        if let Some(existing) = seen.get(&prompt.name) {
            diagnostics.push(ResourceDiagnostic {
                r#type: ResourceDiagnosticType::Collision,
                message: format!("name \"/{}\" collision", prompt.name),
                path: Some(prompt.file_path.clone()),
                collision: Some(ResourceCollision {
                    resource_type: ResourceCollisionResourceType::Prompt,
                    name: prompt.name.clone(),
                    winner_path: existing.file_path.clone(),
                    loser_path: prompt.file_path.clone(),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            seen.insert(prompt.name.clone(), prompt);
        }
    }
    (seen.into_values().collect(), diagnostics)
}

fn dedupe_themes(themes: Vec<Arc<Theme>>) -> (Vec<Arc<Theme>>, Vec<ResourceDiagnostic>) {
    let mut seen: IndexMap<String, Arc<Theme>> = IndexMap::new();
    let mut diagnostics = Vec::new();
    for theme in themes {
        let name = theme.name().unwrap_or_else(|| "unnamed".to_string());
        if let Some(existing) = seen.get(&name) {
            diagnostics.push(ResourceDiagnostic {
                r#type: ResourceDiagnosticType::Collision,
                message: format!("name \"{name}\" collision"),
                path: theme.source_path(),
                collision: Some(ResourceCollision {
                    resource_type: ResourceCollisionResourceType::Theme,
                    name: name.clone(),
                    winner_path: existing
                        .source_path()
                        .unwrap_or_else(|| "<builtin>".to_string()),
                    loser_path: theme.source_path().unwrap_or_else(|| "<builtin>".to_string()),
                    winner_source: None,
                    loser_source: None,
                }),
            });
        } else {
            seen.insert(name, theme);
        }
    }
    (seen.into_values().collect(), diagnostics)
}

fn detect_extension_conflicts(extensions: &[ExtensionRef]) -> Vec<(String, String)> {
    let mut conflicts = Vec::new();

    // Track which extension registered each tool and flag
    let mut tool_owners: IndexMap<String, String> = IndexMap::new();
    let mut flag_owners: IndexMap<String, String> = IndexMap::new();

    for extension in extensions {
        let ext = extension.read();
        for tool_name in ext.tools.keys() {
            match tool_owners.get(tool_name) {
                Some(owner) if *owner != ext.path => conflicts.push((
                    ext.path.clone(),
                    format!("Tool \"{tool_name}\" conflicts with {owner}"),
                )),
                _ => {
                    tool_owners.insert(tool_name.clone(), ext.path.clone());
                }
            }
        }
        for flag_name in ext.flags.keys() {
            match flag_owners.get(flag_name) {
                Some(owner) if *owner != ext.path => conflicts.push((
                    ext.path.clone(),
                    format!("Flag \"--{flag_name}\" conflicts with {owner}"),
                )),
                _ => {
                    flag_owners.insert(flag_name.clone(), ext.path.clone());
                }
            }
        }
    }

    conflicts
}

#[async_trait::async_trait]
impl ResourceLoader for DefaultResourceLoader {
    fn get_extensions(&self) -> LoadExtensionsResult {
        self.state.lock().extensions_result.clone()
    }

    fn get_skills(&self) -> ResourceLoaderGetSkillsResult {
        let state = self.state.lock();
        ResourceLoaderGetSkillsResult {
            skills: state.skills.clone(),
            diagnostics: state.skill_diagnostics.clone(),
        }
    }

    fn get_prompts(&self) -> ResourceLoaderGetPromptsResult {
        let state = self.state.lock();
        ResourceLoaderGetPromptsResult {
            prompts: state.prompts.clone(),
            diagnostics: state.prompt_diagnostics.clone(),
        }
    }

    fn get_themes(&self) -> ResourceLoaderGetThemesResult {
        let state = self.state.lock();
        ResourceLoaderGetThemesResult {
            themes: state.themes.clone(),
            diagnostics: state.theme_diagnostics.clone(),
        }
    }

    fn get_agents_files(&self) -> ResourceLoaderGetAgentsFilesResult {
        ResourceLoaderGetAgentsFilesResult {
            agents_files: self.state.lock().agents_files.clone(),
        }
    }

    fn get_system_prompt(&self) -> Option<String> {
        self.state.lock().system_prompt.clone()
    }

    fn get_system_prompt_source(&self) -> Option<ResourceLoaderGetSystemPromptSourceResult> {
        self.state
            .lock()
            .system_prompt_source_path
            .clone()
            .filter(|p| !p.is_empty())
            .map(|path| ResourceLoaderGetSystemPromptSourceResult { path })
    }

    fn get_append_system_prompt(&self) -> Vec<String> {
        self.state.lock().append_system_prompt.clone()
    }

    fn get_append_system_prompt_sources(
        &self,
    ) -> Vec<ResourceLoaderGetAppendSystemPromptSourcesResult> {
        self.state
            .lock()
            .append_system_prompt_source_paths
            .iter()
            .map(|path| ResourceLoaderGetSystemPromptSourceResult { path: path.clone() })
            .collect()
    }

    fn extend_resources(&self, paths: &ResourceExtensionPaths) {
        let skill_paths = self.normalize_extension_paths(paths.skill_paths.as_deref().unwrap_or(&[]));
        let prompt_paths =
            self.normalize_extension_paths(paths.prompt_paths.as_deref().unwrap_or(&[]));
        let theme_paths = self.normalize_extension_paths(paths.theme_paths.as_deref().unwrap_or(&[]));

        let metadata_by_path = {
            let mut state = self.state.lock();
            for entry in &skill_paths {
                state
                    .extension_skill_source_infos
                    .insert(entry.path.clone(), create_source_info(&entry.path, &entry.metadata));
            }
            for entry in &prompt_paths {
                state
                    .extension_prompt_source_infos
                    .insert(entry.path.clone(), create_source_info(&entry.path, &entry.metadata));
            }
            for entry in &theme_paths {
                state
                    .extension_theme_source_infos
                    .insert(entry.path.clone(), create_source_info(&entry.path, &entry.metadata));
            }
            state.resource_metadata_by_path.clone()
        };

        if !skill_paths.is_empty() {
            let last = self.state.lock().last_skill_paths.clone();
            let merged = self.merge_paths(
                &last,
                &skill_paths.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            );
            self.state.lock().last_skill_paths = merged.clone();
            self.update_skills_from_paths(&merged, Some(&metadata_by_path));
        }

        if !prompt_paths.is_empty() {
            let last = self.state.lock().last_prompt_paths.clone();
            let merged = self.merge_paths(
                &last,
                &prompt_paths.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            );
            self.state.lock().last_prompt_paths = merged.clone();
            self.update_prompts_from_paths(&merged, Some(&metadata_by_path));
        }

        if !theme_paths.is_empty() {
            let last = self.state.lock().last_theme_paths.clone();
            let merged = self.merge_paths(
                &last,
                &theme_paths.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            );
            self.state.lock().last_theme_paths = merged.clone();
            self.update_themes_from_paths(&merged, Some(&metadata_by_path));
        }
    }

    async fn reload(&self, options: Option<&ResourceLoaderReloadOptions>) -> anyhow::Result<()> {
        self.reload_impl(options).await
    }
}

#[cfg(test)]
#[path = "resource_loader_tests.rs"]
mod tests;
