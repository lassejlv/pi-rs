//! Port of `packages/coding-agent/src/core/package-manager.ts`.
//!
//! Packages are installed into the same layout as TS pi so both versions share them:
//! `<agentDir>/npm` (user npm root), `<cwd>/.pi/npm` (project npm root), `<agentDir>/git/<host>/<path>`
//! and `<cwd>/.pi/git/<host>/<path>` (git checkouts), and `<agentDir>/tmp/extensions` for
//! temporary (`-e`) sources.
//!
//! Like TS pi, npm operations shell out to `npm` (or the `npmCommand` setting) and git operations
//! to `git`. When `npmCommand` is not configured and no `npm` executable is on `PATH` (or
//! `PI_BUILTIN_NPM=1` is set), npm packages are installed with the built-in registry client in
//! [`npm_registry`] instead (no lifecycle scripts; see that module).
//!
//! Process execution goes through [`PackageCommandRunner`] so tests can substitute commands, the
//! way the TS tests spy on `runCommand`/`runCommandCapture`/`runCommandSync`.

mod glob;
pub(crate) mod node_semver;
pub(crate) mod npm_registry;
#[cfg(test)]
mod tests;

/// Serializes tests that change process-global state the package manager reads
/// (`HOME`, `PI_OFFLINE`, cwd, `PI_CODING_AGENT_DIR`).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use crate::config::CONFIG_DIR_NAME;
use crate::core::output_guard::is_stdout_taken_over;
use crate::core::pi_manifest::{PiManifest, read_pi_manifest};
use crate::core::settings_manager::{PackageSource, Settings, SettingsManager};
use crate::core::settings_schema::PackageSourceVariant2;
use crate::core::source_info::{BUILTIN_PATH_PREFIX, SourceOrigin};
use crate::modes::interactive::components::config_selector::SettingsScope;
use crate::utils::git::{GitSource, parse_git_url};
use crate::utils::paths::{
    PathInputOptions, canonicalize_path, is_local_path, mark_path_ignored_by_cloud_sync,
    node_basename, node_dirname, node_join, node_relative, node_resolve, normalize_path,
    resolve_path,
};
use crate::utils::text::strip_bom;
use anyhow::{anyhow, bail};
use futures::StreamExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::Arc;

pub use glob::IgnoreMatcher;

const NETWORK_TIMEOUT_MS: u64 = 10000;
const UPDATE_CHECK_CONCURRENCY: usize = 4;
const GIT_UPDATE_CONCURRENCY: usize = 4;

fn is_offline_mode_enabled() -> bool {
    let Ok(value) = std::env::var("PI_OFFLINE") else {
        return false;
    };
    if value.is_empty() {
        return false;
    }
    let lower = value.to_lowercase();
    value == "1" || lower == "true" || lower == "yes"
}

fn is_builtin_npm_forced() -> bool {
    let Ok(value) = std::env::var("PI_BUILTIN_NPM") else {
        return false;
    };
    let lower = value.to_lowercase();
    value == "1" || lower == "true" || lower == "yes"
}

fn is_exact_npm_version(version: Option<&str>) -> bool {
    node_semver::is_valid_version(version.unwrap_or(""))
}

fn get_npm_version_range(version: Option<&str>) -> Option<String> {
    let version = version?;
    if version.is_empty() {
        return None;
    }
    node_semver::valid_range(version)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedResource {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
    /// Packages skipped because installation failed while resolving resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_failures: Option<Vec<PackageInstallFailure>>,
}

/// A configured package that could not be installed while resolving resources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInstallFailure {
    pub source: String,
    pub scope: SourceScope,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MissingSourceAction {
    #[serde(rename = "install")]
    Install,
    #[serde(rename = "skip")]
    Skip,
    #[serde(rename = "error")]
    Error,
}

impl MissingSourceAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Skip => "skip",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProgressEventType {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "progress")]
    Progress,
    #[serde(rename = "complete")]
    Complete,
    #[serde(rename = "error")]
    Error,
}

impl ProgressEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Progress => "progress",
            Self::Complete => "complete",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProgressEventAction {
    #[serde(rename = "install")]
    Install,
    #[serde(rename = "remove")]
    Remove,
    #[serde(rename = "update")]
    Update,
    #[serde(rename = "clone")]
    Clone,
    #[serde(rename = "pull")]
    Pull,
}

impl ProgressEventAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Update => "update",
            Self::Clone => "clone",
            Self::Pull => "pull",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub r#type: ProgressEventType,
    pub action: ProgressEventAction,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub type ProgressCallback = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

/// `onMissing` callback of [`PackageManager::resolve`].
pub type MissingSourceHandler =
    Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<MissingSourceAction>> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageUpdateType {
    #[serde(rename = "npm")]
    Npm,
    #[serde(rename = "git")]
    Git,
}

impl PackageUpdateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Git => "git",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    pub r#type: PackageUpdateType,
    pub scope: SettingsScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: SettingsScope,
    pub filtered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerInstallOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerInstallAndPersistOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerRemoveOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerRemoveAndPersistOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerResolveExtensionSourcesOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerAddSourceToSettingsOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManagerRemoveSourceFromSettingsOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

#[async_trait::async_trait]
pub trait PackageManager: Send + Sync {
    async fn resolve(&self, on_missing: Option<MissingSourceHandler>) -> anyhow::Result<ResolvedPaths>;
    async fn install(
        &self,
        source: &str,
        options: Option<&PackageManagerInstallOptions>,
    ) -> anyhow::Result<()>;
    async fn install_and_persist(
        &self,
        source: &str,
        options: Option<&PackageManagerInstallAndPersistOptions>,
    ) -> anyhow::Result<()>;
    async fn remove(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveOptions>,
    ) -> anyhow::Result<()>;
    async fn remove_and_persist(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveAndPersistOptions>,
    ) -> anyhow::Result<bool>;
    async fn update(&self, source: Option<&str>) -> anyhow::Result<()>;
    fn list_configured_packages(&self) -> Vec<ConfiguredPackage>;
    async fn resolve_extension_sources(
        &self,
        sources: &[String],
        options: Option<&PackageManagerResolveExtensionSourcesOptions>,
    ) -> anyhow::Result<ResolvedPaths>;
    /// Errors where TS throws (project scope while the project is untrusted).
    fn add_source_to_settings(
        &self,
        source: &str,
        options: Option<&PackageManagerAddSourceToSettingsOptions>,
    ) -> anyhow::Result<bool>;
    /// Errors where TS throws (project scope while the project is untrusted).
    fn remove_source_from_settings(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveSourceFromSettingsOptions>,
    ) -> anyhow::Result<bool>;
    fn set_progress_callback(&self, callback: Option<&ProgressCallback>);
    /// Errors where TS throws (project scope while the project is untrusted).
    fn get_installed_path(&self, source: &str, scope: SettingsScope) -> anyhow::Result<Option<String>>;
}

#[derive(Clone)]
pub struct PackageManagerOptions {
    pub cwd: String,
    pub agent_dir: String,
    pub settings_manager: Arc<SettingsManager>,
    /// Names of built-in extensions, resolved as `builtin:<name>` extension resources.
    pub builtin_extensions: Option<Vec<String>>,
}

/// TS: a local `SourceScope` alias identical to `source-info.ts`.
pub use crate::core::source_info::SourceScope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmSource {
    /// Always "npm".
    pub r#type: String,
    pub spec: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSource {
    /// Always "local".
    pub r#type: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParsedSource {
    NpmSource(NpmSource),
    GitSource(GitSource),
    LocalSource(LocalSource),
}

pub type InstalledSourceScope = SettingsScope;

fn installed_to_source_scope(scope: InstalledSourceScope) -> SourceScope {
    match scope {
        SettingsScope::User => SourceScope::User,
        SettingsScope::Project => SourceScope::Project,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfiguredUpdateSource {
    pub source: String,
    pub scope: InstalledSourceScope,
}

// TS: extends ConfiguredUpdateSource (inherited fields inlined)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmUpdateTarget {
    pub source: String,
    pub scope: InstalledSourceScope,
    pub parsed: NpmSource,
}

// TS: extends ConfiguredUpdateSource (inherited fields inlined)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitUpdateTarget {
    pub source: String,
    pub scope: InstalledSourceScope,
    pub parsed: GitSource,
}

/// A resource collected while resolving (`{ metadata, enabled }` map values in TS).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceEntry {
    pub metadata: PathMetadata,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceAccumulator {
    pub extensions: IndexMap<String, ResourceEntry>,
    pub skills: IndexMap<String, ResourceEntry>,
    pub prompts: IndexMap<String, ResourceEntry>,
    pub themes: IndexMap<String, ResourceEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

impl PackageFilter {
    fn from_source(pkg: &PackageSourceVariant2) -> Self {
        Self {
            autoload: pkg.autoload,
            extensions: pkg.extensions.clone(),
            skills: pkg.skills.clone(),
            prompts: pkg.prompts.clone(),
            themes: pkg.themes.clone(),
        }
    }

    fn patterns(&self, resource_type: ResourceType) -> Option<&Vec<String>> {
        match resource_type {
            ResourceType::Extensions => self.extensions.as_ref(),
            ResourceType::Skills => self.skills.as_ref(),
            ResourceType::Prompts => self.prompts.as_ref(),
            ResourceType::Themes => self.themes.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceType {
    #[serde(rename = "extensions")]
    Extensions,
    #[serde(rename = "skills")]
    Skills,
    #[serde(rename = "prompts")]
    Prompts,
    #[serde(rename = "themes")]
    Themes,
}

impl ResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    /// `FILE_PATTERNS[resourceType]`.
    fn matches_file_name(&self, name: &str) -> bool {
        match self {
            Self::Extensions => name.ends_with(".ts") || name.ends_with(".js"),
            Self::Skills | Self::Prompts => name.ends_with(".md"),
            Self::Themes => name.ends_with(".json"),
        }
    }

    fn manifest_entries<'a>(&self, manifest: &'a PiManifest) -> Option<&'a Vec<String>> {
        match self {
            Self::Extensions => manifest.extensions.as_ref(),
            Self::Skills => manifest.skills.as_ref(),
            Self::Prompts => manifest.prompts.as_ref(),
            Self::Themes => manifest.themes.as_ref(),
        }
    }

    fn settings_entries(&self, settings: &Settings) -> Vec<String> {
        match self {
            Self::Extensions => settings.extensions.clone(),
            Self::Skills => settings.skills.clone(),
            Self::Prompts => settings.prompts.clone(),
            Self::Themes => settings.themes.clone(),
        }
        .unwrap_or_default()
    }
}

const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

fn to_posix_path(p: &str) -> String {
    if MAIN_SEPARATOR == '/' {
        p.to_string()
    } else {
        p.split(MAIN_SEPARATOR).collect::<Vec<_>>().join("/")
    }
}

fn get_home_dir() -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => home,
        _ => dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn get_extension_temp_folder(agent_dir: &str) -> String {
    let temp_folder = node_join(&[agent_dir, "tmp", "extensions"]);
    let _ = std::fs::create_dir_all(&temp_folder);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp_folder, std::fs::Permissions::from_mode(0o700));
    }
    temp_folder
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line;
    let mut negated = false;

    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if pattern.starts_with("\\!") {
        pattern = &pattern[1..];
    }

    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }

    let prefixed = format!("{prefix}{pattern}");
    Some(if negated { format!("!{prefixed}") } else { prefixed })
}

fn add_ignore_rules(ig: &mut IgnoreMatcher, dir: &str, root_dir: &str) {
    let relative_dir = node_relative(root_dir, dir);
    let prefix = if relative_dir.is_empty() {
        String::new()
    } else {
        format!("{}/", to_posix_path(&relative_dir))
    };

    for filename in IGNORE_FILE_NAMES {
        let ignore_path = node_join(&[dir, filename]);
        if !exists(&ignore_path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&ignore_path) else {
            continue;
        };
        let patterns: Vec<String> = content
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            ig.add(patterns);
        }
    }
}

fn is_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-') || s.contains('*') || s.contains('?')
}

fn is_override_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

/// Glob entries discover visible paths; exact entries can target dot paths or symlinked trees.
fn expand_package_glob(pattern: &str, root: &str) -> Vec<String> {
    let mut matches: Vec<String> = glob::glob_sync(pattern, Path::new(root))
        .into_iter()
        .map(|m| node_resolve(&[root, &m]))
        .filter(|path| {
            node_relative(root, path)
                .split(MAIN_SEPARATOR)
                .all(|segment| segment == ".." || !segment.starts_with('.'))
        })
        .collect();
    matches.sort();
    matches
}

fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

/// `readdirSync(dir, { withFileTypes: true })` resolved through symlinks: `(name, path, isDir, isFile)`.
/// Broken symlinks are skipped. Entries come in libuv's scandir order (sorted by byte value).
fn read_dir_entries(dir: &str) -> Option<Vec<(String, String, bool, bool)>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    let mut out = Vec::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_path = node_join(&[dir, &name]);
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let (is_dir, is_file) = if file_type.is_symlink() {
            match std::fs::metadata(&full_path) {
                Ok(meta) => (meta.is_dir(), meta.is_file()),
                Err(_) => continue,
            }
        } else {
            (file_type.is_dir(), file_type.is_file())
        };
        out.push((name, full_path, is_dir, is_file));
    }
    Some(out)
}

fn collect_files(
    dir: &str,
    resource_type: ResourceType,
    skip_node_modules: bool,
    ignore_matcher: Option<&mut IgnoreMatcher>,
    root_dir: Option<&str>,
) -> Vec<String> {
    let mut files = Vec::new();
    if !exists(dir) {
        return files;
    }

    let root = root_dir.unwrap_or(dir).to_string();
    let mut own = None;
    let ig = match ignore_matcher {
        Some(ig) => ig,
        None => own.insert(IgnoreMatcher::new(Path::new(&root))),
    };
    add_ignore_rules(ig, dir, &root);

    let Some(entries) = read_dir_entries(dir) else {
        return files;
    };
    for (name, full_path, is_dir, is_file) in entries {
        if name.starts_with('.') {
            continue;
        }
        if skip_node_modules && name == "node_modules" {
            continue;
        }
        let rel_path = to_posix_path(&node_relative(&root, &full_path));
        let ignore_path = if is_dir { format!("{rel_path}/") } else { rel_path };
        if ig.ignores(&ignore_path) {
            continue;
        }
        if is_dir {
            files.extend(collect_files(
                &full_path,
                resource_type,
                skip_node_modules,
                Some(ig),
                Some(&root),
            ));
        } else if is_file && resource_type.matches_file_name(&name) {
            files.push(full_path);
        }
    }
    files
}

fn collect_skill_entries(
    dir: &str,
    mode: SkillDiscoveryMode,
    ignore_matcher: Option<&mut IgnoreMatcher>,
    root_dir: Option<&str>,
) -> Vec<String> {
    let mut entries = Vec::new();
    if !exists(dir) {
        return entries;
    }

    let root = root_dir.unwrap_or(dir).to_string();
    let mut own = None;
    let ig = match ignore_matcher {
        Some(ig) => ig,
        None => own.insert(IgnoreMatcher::new(Path::new(&root))),
    };
    add_ignore_rules(ig, dir, &root);

    let Some(dir_entries) = read_dir_entries(dir) else {
        return entries;
    };

    for (name, full_path, _, is_file) in &dir_entries {
        if name != "SKILL.md" {
            continue;
        }
        let rel_path = to_posix_path(&node_relative(&root, full_path));
        if *is_file && !ig.ignores(&rel_path) {
            entries.push(full_path.clone());
            return entries;
        }
    }

    for (name, full_path, is_dir, is_file) in dir_entries {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let rel_path = to_posix_path(&node_relative(&root, &full_path));
        let should_include_markdown_file = is_file
            && name.ends_with(".md")
            && !ig.ignores(&rel_path)
            && ((mode == SkillDiscoveryMode::Pi && dir == root)
                || (mode == SkillDiscoveryMode::Agents && dir != root));
        if should_include_markdown_file {
            entries.push(full_path);
            continue;
        }
        if !is_dir {
            continue;
        }
        if ig.ignores(&format!("{rel_path}/")) {
            continue;
        }
        entries.extend(collect_skill_entries(&full_path, mode, Some(ig), Some(&root)));
    }

    entries
}

fn collect_auto_skill_entries(dir: &str, mode: SkillDiscoveryMode) -> Vec<String> {
    collect_skill_entries(dir, mode, None, None)
}

fn find_git_repo_root(start_dir: &str) -> Option<String> {
    let mut dir = node_resolve(&[start_dir]);
    loop {
        if exists(&node_join(&[&dir, ".git"])) {
            return Some(dir);
        }
        let parent = node_dirname(&dir);
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

fn collect_ancestor_agents_skill_dirs(start_dir: &str) -> Vec<String> {
    let mut skill_dirs = Vec::new();
    let resolved_start_dir = node_resolve(&[start_dir]);
    let git_repo_root = find_git_repo_root(&resolved_start_dir);

    let mut dir = resolved_start_dir;
    loop {
        skill_dirs.push(node_join(&[&dir, ".agents", "skills"]));
        if git_repo_root.as_deref() == Some(dir.as_str()) {
            break;
        }
        let parent = node_dirname(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }
    skill_dirs
}

fn collect_auto_flat_entries(dir: &str, extension: &str) -> Vec<String> {
    let mut entries = Vec::new();
    if !exists(dir) {
        return entries;
    }
    let mut ig = IgnoreMatcher::new(Path::new(dir));
    add_ignore_rules(&mut ig, dir, dir);

    let Some(dir_entries) = read_dir_entries(dir) else {
        return entries;
    };
    for (name, full_path, _, is_file) in dir_entries {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let rel_path = to_posix_path(&node_relative(dir, &full_path));
        if ig.ignores(&rel_path) {
            continue;
        }
        if is_file && name.ends_with(extension) {
            entries.push(full_path);
        }
    }
    entries
}

fn collect_auto_prompt_entries(dir: &str) -> Vec<String> {
    collect_auto_flat_entries(dir, ".md")
}

fn collect_auto_theme_entries(dir: &str) -> Vec<String> {
    collect_auto_flat_entries(dir, ".json")
}

fn resolve_extension_entries(dir: &str) -> Option<Vec<String>> {
    let package_json_path = node_join(&[dir, "package.json"]);
    if exists(&package_json_path)
        && let Some(manifest) = read_pi_manifest(&package_json_path)
        && let Some(extensions) = manifest.extensions.filter(|e| !e.is_empty())
    {
        let entries: Vec<String> = extensions
            .iter()
            .map(|ext_path| node_resolve(&[dir, ext_path]))
            .filter(|p| exists(p))
            .collect();
        if !entries.is_empty() {
            return Some(entries);
        }
    }

    let index_ts = node_join(&[dir, "index.ts"]);
    let index_js = node_join(&[dir, "index.js"]);
    if exists(&index_ts) {
        return Some(vec![index_ts]);
    }
    if exists(&index_js) {
        return Some(vec![index_js]);
    }
    None
}

fn collect_auto_extension_entries(dir: &str) -> Vec<String> {
    let mut entries = Vec::new();
    if !exists(dir) {
        return entries;
    }

    // First check if this directory itself has explicit extension entries (package.json or index)
    if let Some(root_entries) = resolve_extension_entries(dir) {
        return root_entries;
    }

    // Otherwise, discover extensions from directory contents
    let mut ig = IgnoreMatcher::new(Path::new(dir));
    add_ignore_rules(&mut ig, dir, dir);

    let Some(dir_entries) = read_dir_entries(dir) else {
        return entries;
    };
    for (name, full_path, is_dir, is_file) in dir_entries {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let rel_path = to_posix_path(&node_relative(dir, &full_path));
        let ignore_path = if is_dir { format!("{rel_path}/") } else { rel_path };
        if ig.ignores(&ignore_path) {
            continue;
        }
        if is_file && (name.ends_with(".ts") || name.ends_with(".js")) {
            entries.push(full_path);
        } else if is_dir && let Some(resolved) = resolve_extension_entries(&full_path) {
            entries.extend(resolved);
        }
    }
    entries
}

/// Collect resource files from a directory based on resource type.
/// Extensions use smart discovery (index.ts in subdirs), others use recursive collection.
fn collect_resource_files(dir: &str, resource_type: ResourceType) -> Vec<String> {
    match resource_type {
        ResourceType::Skills => collect_skill_entries(dir, SkillDiscoveryMode::Pi, None, None),
        ResourceType::Extensions => collect_auto_extension_entries(dir),
        _ => collect_files(dir, resource_type, true, None, None),
    }
}

struct MatchTarget {
    rel: String,
    name: String,
    file_path_posix: String,
    /// `(parentRel, parentName, parentDirPosix)` for `SKILL.md` files.
    skill_parent: Option<(String, String, String)>,
}

fn match_target(file_path: &str, base_dir: &str) -> MatchTarget {
    let rel = to_posix_path(&node_relative(base_dir, file_path));
    let name = node_basename(file_path);
    let file_path_posix = to_posix_path(file_path);
    let skill_parent = (name == "SKILL.md").then(|| {
        let parent_dir = node_dirname(file_path);
        (
            to_posix_path(&node_relative(base_dir, &parent_dir)),
            node_basename(&parent_dir),
            to_posix_path(&parent_dir),
        )
    });
    MatchTarget {
        rel,
        name,
        file_path_posix,
        skill_parent,
    }
}

fn matches_any_pattern(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    let target = match_target(file_path, base_dir);
    patterns.iter().any(|pattern| {
        let normalized_pattern = to_posix_path(&normalize_path(pattern, None));
        if glob::minimatch(&target.rel, &normalized_pattern)
            || glob::minimatch(&target.name, &normalized_pattern)
            || glob::minimatch(&target.file_path_posix, &normalized_pattern)
        {
            return true;
        }
        let Some((parent_rel, parent_name, parent_dir_posix)) = &target.skill_parent else {
            return false;
        };
        glob::minimatch(parent_rel, &normalized_pattern)
            || glob::minimatch(parent_name, &normalized_pattern)
            || glob::minimatch(parent_dir_posix, &normalized_pattern)
    })
}

fn normalize_exact_pattern(pattern: &str) -> String {
    let normalized = if pattern.starts_with("./") || pattern.starts_with(".\\") {
        &pattern[2..]
    } else {
        pattern
    };
    to_posix_path(&normalize_path(normalized, None))
}

fn matches_any_exact_pattern(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let target = match_target(file_path, base_dir);
    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        if normalized == target.rel || normalized == target.file_path_posix {
            return true;
        }
        let Some((parent_rel, _, parent_dir_posix)) = &target.skill_parent else {
            return false;
        };
        normalized == *parent_rel || normalized == *parent_dir_posix
    })
}

fn get_override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| is_override_pattern(p))
        .cloned()
        .collect()
}

fn is_enabled_by_overrides(file_path: &str, patterns: &[String], base_dir: &str) -> bool {
    let overrides = get_override_patterns(patterns);
    let strip = |prefix: char| -> Vec<String> {
        overrides
            .iter()
            .filter_map(|p| p.strip_prefix(prefix).map(str::to_string))
            .collect()
    };
    let excludes = strip('!');
    let force_includes = strip('+');
    let force_excludes = strip('-');

    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(file_path, &excludes, base_dir) {
        enabled = false;
    }
    if !force_includes.is_empty() && matches_any_exact_pattern(file_path, &force_includes, base_dir) {
        enabled = true;
    }
    if !force_excludes.is_empty() && matches_any_exact_pattern(file_path, &force_excludes, base_dir) {
        enabled = false;
    }
    enabled
}

/// Apply patterns to paths and return the enabled paths.
/// Pattern types:
/// - Plain patterns: include matching paths
/// - `!pattern`: exclude matching paths
/// - `+path`: force-include exact path (overrides exclusions)
/// - `-path`: force-exclude exact path (overrides force-includes)
fn apply_patterns(all_paths: &[String], patterns: &[String], base_dir: &str) -> Vec<String> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();

    for p in patterns {
        if let Some(rest) = p.strip_prefix('+') {
            force_includes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('-') {
            force_excludes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('!') {
            excludes.push(rest.to_string());
        } else {
            includes.push(p.clone());
        }
    }

    // Step 1: Apply includes (or all if no includes)
    let mut result: Vec<String> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|f| matches_any_pattern(f, &includes, base_dir))
            .cloned()
            .collect()
    };

    // Step 2: Apply excludes
    if !excludes.is_empty() {
        result.retain(|f| !matches_any_pattern(f, &excludes, base_dir));
    }

    // Step 3: Force-include (add back from allPaths, overriding exclusions)
    if !force_includes.is_empty() {
        for file_path in all_paths {
            if !result.contains(file_path)
                && matches_any_exact_pattern(file_path, &force_includes, base_dir)
            {
                result.push(file_path.clone());
            }
        }
    }

    // Step 4: Force-exclude (remove even if included or force-included)
    if !force_excludes.is_empty() {
        result.retain(|f| !matches_any_exact_pattern(f, &force_excludes, base_dir));
    }

    // Set semantics: drop duplicates, keep first insertion order.
    let mut seen = HashSet::new();
    result.retain(|p| seen.insert(p.clone()));
    result
}

fn apply_autoload_disabled_patterns(
    all_paths: &[String],
    patterns: &[String],
    base_dir: &str,
) -> IndexMap<String, bool> {
    let mut result = IndexMap::new();
    for pattern in patterns {
        let has_prefix = pattern.starts_with('+') || pattern.starts_with('-') || pattern.starts_with('!');
        let target = if has_prefix { &pattern[1..] } else { pattern.as_str() };
        let enabled = !pattern.starts_with('-') && !pattern.starts_with('!');
        let exact = pattern.starts_with('+') || pattern.starts_with('-');
        let targets = [target.to_string()];
        for file_path in all_paths {
            let matched = if exact {
                matches_any_exact_pattern(file_path, &targets, base_dir)
            } else {
                matches_any_pattern(file_path, &targets, base_dir)
            };
            if matched {
                result.insert(file_path.clone(), enabled);
            }
        }
    }
    result
}

/// Compute a numeric precedence rank for a resource based on its metadata.
/// Lower rank = higher precedence. Used to sort resolved resources so that
/// name-collision resolution ("first wins") produces the correct outcome.
///
/// Precedence (highest to lowest):
///   0  project + settings entry (source: "local", scope: "project")
///   1  project + auto-discovered (source: "auto", scope: "project")
///   2  user + settings entry (source: "local", scope: "user")
///   3  user + auto-discovered (source: "auto", scope: "user")
///   4  package resource (origin: "package")
fn resource_precedence_rank(m: &PathMetadata) -> u8 {
    // Built-in extensions load after file and package extensions, whichever scope enables them.
    if m.source == "builtin" {
        return 5;
    }
    if m.origin == SourceOrigin::Package {
        return 4;
    }
    let scope_base = if m.scope == SourceScope::Project { 0 } else { 2 };
    scope_base + if m.source == "local" { 0 } else { 1 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SkillDiscoveryMode {
    #[serde(rename = "pi")]
    Pi,
    #[serde(rename = "agents")]
    Agents,
}

impl SkillDiscoveryMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Agents => "agents",
        }
    }
}

fn package_source_string(pkg: &PackageSource) -> &str {
    match pkg {
        PackageSource::String(s) => s,
        PackageSource::PackageSourceVariant2(obj) => &obj.source,
    }
}

fn package_filter(pkg: &PackageSource) -> Option<PackageFilter> {
    match pkg {
        PackageSource::String(_) => None,
        PackageSource::PackageSourceVariant2(obj) => Some(PackageFilter::from_source(obj)),
    }
}

// ============================================================================
// Command execution
// ============================================================================

/// Options for [`PackageCommandRunner::run_capture`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Extra environment variables layered over the process environment.
    pub env: Option<Vec<(String, String)>>,
}

/// Runs external commands (`npm`, `git`, ...) for the package manager.
#[async_trait::async_trait]
pub trait PackageCommandRunner: Send + Sync {
    /// `runCommand`: inherit stdio (stderr when stdout is taken over); fail on non-zero exit.
    async fn run(&self, command: &str, args: &[String], cwd: Option<&str>) -> anyhow::Result<()>;
    /// `runCommandCapture`: capture stdout (trimmed); fail on non-zero exit or timeout.
    async fn run_capture(
        &self,
        command: &str,
        args: &[String],
        options: &CaptureOptions,
    ) -> anyhow::Result<String>;
    /// `runCommandSync`: blocking capture of stdout (or stderr), trimmed.
    fn run_sync(&self, command: &str, args: &[String]) -> anyhow::Result<String>;
    /// Whether `command` can be executed (used to decide on the built-in npm client).
    fn command_exists(&self, command: &str) -> bool {
        find_executable(command).is_some()
    }
}

/// Locate an executable on `PATH` (honoring `PATHEXT` on Windows).
pub fn find_executable(command: &str) -> Option<PathBuf> {
    let is_executable = |path: &Path| -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let mut out = vec![base.clone()];
        for ext in &extensions {
            let mut with_ext = base.clone().into_os_string();
            with_ext.push(ext);
            out.push(PathBuf::from(with_ext));
        }
        out
    };
    if command.contains('/') || (cfg!(windows) && command.contains('\\')) {
        return candidates(PathBuf::from(command)).into_iter().find(|p| is_executable(p));
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in candidates(dir.join(command)) {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("signal {signal}"),
    }
    .to_string()
}

fn spawn_error(command: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow!("spawn {command} ENOENT")
    } else {
        anyhow!("spawn {command} {error}")
    }
}

fn build_std_command(command: &str, args: &[String]) -> std::process::Command {
    #[cfg(windows)]
    {
        // `npm` and friends are `.cmd` shims on Windows; run them through cmd.exe.
        if let Some(resolved) = find_executable(command) {
            let lower = resolved.to_string_lossy().to_lowercase();
            if lower.ends_with(".cmd") || lower.ends_with(".bat") {
                let mut cmd = crate::utils::child_process::spawn_process_sync("cmd.exe", &[]);
                cmd.arg("/d").arg("/s").arg("/c").arg(&resolved).args(args);
                return cmd;
            }
            return crate::utils::child_process::spawn_process_sync(&resolved.to_string_lossy(), args);
        }
    }
    crate::utils::child_process::spawn_process_sync(command, args)
}

/// The default runner: spawns real processes.
pub struct ProcessCommandRunner;

#[async_trait::async_trait]
impl PackageCommandRunner for ProcessCommandRunner {
    async fn run(&self, command: &str, args: &[String], cwd: Option<&str>) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::from(build_std_command(command, args));
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if is_stdout_taken_over() {
            cmd.stdin(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::fd::AsFd;
                if let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() {
                    cmd.stdout(std::process::Stdio::from(fd));
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsHandle;
                if let Ok(handle) = std::io::stderr().as_handle().try_clone_to_owned() {
                    cmd.stdout(std::process::Stdio::from(handle));
                }
            }
        }
        let status = cmd
            .status()
            .await
            .map_err(|error| spawn_error(command, error))?;
        if status.success() {
            return Ok(());
        }
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".to_string());
        bail!("{command} {} failed with code {code}", args.join(" "))
    }

    async fn run_capture(
        &self,
        command: &str,
        args: &[String],
        options: &CaptureOptions,
    ) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::from(build_std_command(command, args));
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = &options.env {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|error| spawn_error(command, error))?;
        let output = match options.timeout_ms {
            Some(timeout_ms) => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    child.wait_with_output(),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => bail!(
                        "{command} {} timed out after {timeout_ms}ms",
                        args.join(" ")
                    ),
                }
            }
            None => child.wait_with_output().await?,
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.status.success() {
            return Ok(stdout.trim().to_string());
        }
        let exit_status = match output.status.code() {
            Some(code) => format!("code {code}"),
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    match output.status.signal() {
                        Some(signal) => signal_name(signal),
                        None => "signal unknown".to_string(),
                    }
                }
                #[cfg(not(unix))]
                {
                    "signal unknown".to_string()
                }
            }
        };
        let exit_status = if exit_status.starts_with("SIG") {
            format!("signal {exit_status}")
        } else {
            exit_status
        };
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "{command} {} failed with {exit_status}: {detail}",
            args.join(" ")
        )
    }

    fn run_sync(&self, command: &str, args: &[String]) -> anyhow::Result<String> {
        let output = build_std_command(command, args)
            .stdin(std::process::Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                let message = spawn_error(command, error);
                bail!("Failed to run {command} {}: {message}", args.join(" "));
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            bail!("Failed to run {command} {}: {detail}", args.join(" "));
        }
        let text = if !stdout.is_empty() { stdout } else { stderr };
        Ok(text.trim().to_string())
    }
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The `npm` invocation: the configured `npmCommand` argv, or plain `npm`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmCommand {
    command: String,
    args: Vec<String>,
}

/// `npm install --omit=dev` in a package directory with pi's built-in (pure-Rust) npm client,
/// the fallback pi uses when `npm` is not on `PATH`. Installs the dependencies of `dir`'s
/// `package.json` into `dir/node_modules`.
pub async fn install_dependencies_with_builtin_npm(dir: &Path) -> anyhow::Result<()> {
    npm_registry::NpmRegistryClient::new(&[dir.to_path_buf()]).install_dependencies(dir).await
}

/// TS: `implements PackageManager`
pub struct DefaultPackageManager {
    cwd: String,
    agent_dir: String,
    settings_manager: Arc<SettingsManager>,
    builtin_extensions: Vec<String>,
    /// `(commandKey, globalNpmRoot)` cache.
    global_npm_root: Mutex<Option<(String, String)>>,
    progress_callback: RwLock<Option<ProgressCallback>>,
    runner: Arc<dyn PackageCommandRunner>,
}

impl DefaultPackageManager {
    pub fn new(options: &PackageManagerOptions) -> Self {
        Self::with_command_runner(options, Arc::new(ProcessCommandRunner))
    }

    /// Like [`DefaultPackageManager::new`] with a custom command runner.
    pub fn with_command_runner(
        options: &PackageManagerOptions,
        runner: Arc<dyn PackageCommandRunner>,
    ) -> Self {
        Self {
            cwd: resolve_path(&options.cwd, None, None),
            agent_dir: resolve_path(&options.agent_dir, None, None),
            settings_manager: options.settings_manager.clone(),
            builtin_extensions: options.builtin_extensions.clone().unwrap_or_default(),
            global_npm_root: Mutex::new(None),
            progress_callback: RwLock::new(None),
            runner,
        }
    }

    fn emit_progress(&self, event: ProgressEvent) {
        let callback = self.progress_callback.read().clone();
        if let Some(callback) = callback {
            callback(event);
        }
    }

    async fn with_progress<F>(
        &self,
        action: ProgressEventAction,
        source: &str,
        message: &str,
        operation: F,
    ) -> anyhow::Result<()>
    where
        F: std::future::Future<Output = anyhow::Result<()>>,
    {
        self.emit_progress(ProgressEvent {
            r#type: ProgressEventType::Start,
            action,
            source: source.to_string(),
            message: Some(message.to_string()),
        });
        match operation.await {
            Ok(()) => {
                self.emit_progress(ProgressEvent {
                    r#type: ProgressEventType::Complete,
                    action,
                    source: source.to_string(),
                    message: None,
                });
                Ok(())
            }
            Err(error) => {
                self.emit_progress(ProgressEvent {
                    r#type: ProgressEventType::Error,
                    action,
                    source: source.to_string(),
                    message: Some(error.to_string()),
                });
                Err(error)
            }
        }
    }

    fn scope_for_local(local: Option<bool>) -> SourceScope {
        if local.unwrap_or(false) {
            SourceScope::Project
        } else {
            SourceScope::User
        }
    }

    fn configured_packages(&self, scope: SourceScope) -> Vec<PackageSource> {
        let settings = if scope == SourceScope::Project {
            self.settings_manager.get_project_settings()
        } else {
            self.settings_manager.get_global_settings()
        };
        settings.packages.unwrap_or_default()
    }

    fn write_packages(&self, scope: SourceScope, packages: &[PackageSource]) -> anyhow::Result<()> {
        if scope == SourceScope::Project {
            self.settings_manager.set_project_packages(packages)
        } else {
            self.settings_manager.set_packages(packages);
            Ok(())
        }
    }

    fn add_source_to_settings_impl(&self, source: &str, local: Option<bool>) -> anyhow::Result<bool> {
        let scope = Self::scope_for_local(local);
        let current_packages = self.configured_packages(scope);
        let normalized_source = self.normalize_package_source_for_settings(source, scope)?;
        let mut match_index = None;
        for (index, existing) in current_packages.iter().enumerate() {
            if self.package_sources_match(existing, source, scope)? {
                match_index = Some(index);
                break;
            }
        }
        if let Some(index) = match_index {
            let existing = &current_packages[index];
            if package_source_string(existing) == normalized_source {
                return Ok(false);
            }
            let mut next_packages = current_packages.clone();
            next_packages[index] = match existing {
                PackageSource::String(_) => PackageSource::String(normalized_source),
                PackageSource::PackageSourceVariant2(obj) => {
                    let mut obj = obj.clone();
                    obj.source = normalized_source;
                    PackageSource::PackageSourceVariant2(obj)
                }
            };
            self.write_packages(scope, &next_packages)?;
            return Ok(true);
        }
        let mut next_packages = current_packages;
        next_packages.push(PackageSource::String(normalized_source));
        self.write_packages(scope, &next_packages)?;
        Ok(true)
    }

    fn remove_source_from_settings_impl(&self, source: &str, local: Option<bool>) -> anyhow::Result<bool> {
        let scope = Self::scope_for_local(local);
        let current_packages = self.configured_packages(scope);
        let mut next_packages = Vec::new();
        for existing in &current_packages {
            if !self.package_sources_match(existing, source, scope)? {
                next_packages.push(existing.clone());
            }
        }
        if next_packages.len() == current_packages.len() {
            return Ok(false);
        }
        self.write_packages(scope, &next_packages)?;
        Ok(true)
    }

    fn get_installed_path_impl(&self, source: &str, scope: SourceScope) -> anyhow::Result<Option<String>> {
        let path = match self.parse_source(source) {
            ParsedSource::NpmSource(parsed) => self.get_npm_install_path(&parsed, scope)?,
            ParsedSource::GitSource(parsed) => self.get_git_install_path(&parsed, scope)?,
            ParsedSource::LocalSource(parsed) => {
                let base_dir = self.get_base_dir_for_scope(scope)?;
                self.resolve_path_from_base(&parsed.path, &base_dir)
            }
        };
        Ok(exists(&path).then_some(path))
    }

    async fn resolve_impl(&self, on_missing: Option<MissingSourceHandler>) -> anyhow::Result<ResolvedPaths> {
        let mut accumulator = ResourceAccumulator::default();
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();

        // Collect all packages with scope (project first so cwd resources win collisions)
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in project_settings.packages.clone().unwrap_or_default() {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in global_settings.packages.clone().unwrap_or_default() {
            all_packages.push((pkg, SourceScope::User));
        }

        // Dedupe: project scope wins over global for same package identity
        let package_sources = self.dedupe_packages(all_packages)?;
        let install_failures = self
            .resolve_package_sources(&package_sources, &mut accumulator, on_missing)
            .await?;

        let global_base_dir = self.agent_dir.clone();
        let project_base_dir = node_join(&[&self.cwd, CONFIG_DIR_NAME]);

        for resource_type in RESOURCE_TYPES {
            let global_entries = resource_type.settings_entries(&global_settings);
            let project_entries = resource_type.settings_entries(&project_settings);
            let target = Self::get_target_map(&mut accumulator, resource_type);
            self.resolve_local_entries(
                &project_entries,
                resource_type,
                target,
                &PathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                    package_root: None,
                },
                &project_base_dir,
            );
            self.resolve_local_entries(
                &global_entries,
                resource_type,
                target,
                &PathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                    package_root: None,
                },
                &global_base_dir,
            );
        }

        self.add_auto_discovered_resources(
            &mut accumulator,
            &global_settings,
            &project_settings,
            &global_base_dir,
            &project_base_dir,
        );

        // Built-in extensions are enabled unless the user `extensions` setting excludes them, for example
        // with `-builtin:mcp`. A matching `+`, `-`, or `!` entry in the project setting overrides that.
        let project_extensions = project_settings.extensions.clone().unwrap_or_default();
        let global_extensions = global_settings.extensions.clone().unwrap_or_default();
        for name in &self.builtin_extensions {
            let path = format!("{BUILTIN_PATH_PREFIX}{name}");
            let project_enabled = apply_autoload_disabled_patterns(
                std::slice::from_ref(&path),
                &get_override_patterns(&project_extensions),
                &project_base_dir,
            )
            .get(&path)
            .copied();
            let enabled = project_enabled
                .unwrap_or_else(|| is_enabled_by_overrides(&path, &global_extensions, &global_base_dir));
            Self::add_resource(
                &mut accumulator.extensions,
                &path,
                &PathMetadata {
                    source: "builtin".to_string(),
                    scope: if project_enabled.is_none() {
                        SourceScope::User
                    } else {
                        SourceScope::Project
                    },
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                    package_root: None,
                },
                enabled,
            );
        }

        Ok(ResolvedPaths {
            install_failures: Some(install_failures),
            ..Self::to_resolved_paths(accumulator)
        })
    }

    async fn resolve_extension_sources_impl(
        &self,
        sources: &[String],
        local: Option<bool>,
        temporary: Option<bool>,
    ) -> anyhow::Result<ResolvedPaths> {
        let mut accumulator = ResourceAccumulator::default();
        let scope = if temporary.unwrap_or(false) {
            SourceScope::Temporary
        } else if local.unwrap_or(false) {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        // `-e builtin:<name>` loads a built-in extension; the resource loader reports unknown names.
        for source in sources.iter().filter(|s| s.starts_with(BUILTIN_PATH_PREFIX)) {
            Self::add_resource(
                &mut accumulator.extensions,
                source,
                &PathMetadata {
                    source: "builtin".to_string(),
                    scope,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                    package_root: None,
                },
                true,
            );
        }
        let package_sources: Vec<(PackageSource, SourceScope)> = sources
            .iter()
            .filter(|s| !s.starts_with(BUILTIN_PATH_PREFIX))
            .map(|s| (PackageSource::String(s.clone()), scope))
            .collect();
        let install_failures = self
            .resolve_package_sources(&package_sources, &mut accumulator, None)
            .await?;
        Ok(ResolvedPaths {
            install_failures: Some(install_failures),
            ..Self::to_resolved_paths(accumulator)
        })
    }

    fn list_configured_packages_impl(&self) -> Vec<ConfiguredPackage> {
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();
        let mut configured = Vec::new();
        for (packages, scope) in [
            (global_settings.packages.unwrap_or_default(), SettingsScope::User),
            (project_settings.packages.unwrap_or_default(), SettingsScope::Project),
        ] {
            for pkg in packages {
                let source = package_source_string(&pkg).to_string();
                let installed_path = self
                    .get_installed_path_impl(&source, installed_to_source_scope(scope))
                    .ok()
                    .flatten();
                configured.push(ConfiguredPackage {
                    filtered: matches!(pkg, PackageSource::PackageSourceVariant2(_)),
                    source,
                    scope,
                    installed_path,
                });
            }
        }
        configured
    }

    async fn install_impl(&self, source: &str, local: Option<bool>) -> anyhow::Result<()> {
        let parsed = self.parse_source(source);
        let scope = Self::scope_for_local(local);
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressEventAction::Install,
            source,
            &format!("Installing {source}..."),
            async {
                match &parsed {
                    ParsedSource::NpmSource(npm) => self.install_npm(npm, scope, false).await,
                    ParsedSource::GitSource(git) => self.install_git(git, scope).await,
                    ParsedSource::LocalSource(local) => {
                        let resolved = self.resolve_path(&local.path);
                        if !exists(&resolved) {
                            bail!("Path does not exist: {resolved}");
                        }
                        Ok(())
                    }
                }
            },
        )
        .await
    }

    async fn remove_impl(&self, source: &str, local: Option<bool>) -> anyhow::Result<()> {
        let parsed = self.parse_source(source);
        let scope = Self::scope_for_local(local);
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressEventAction::Remove,
            source,
            &format!("Removing {source}..."),
            async {
                match &parsed {
                    ParsedSource::NpmSource(npm) => self.uninstall_npm(npm, scope).await,
                    ParsedSource::GitSource(git) => self.remove_git(git, scope),
                    ParsedSource::LocalSource(_) => Ok(()),
                }
            },
        )
        .await
    }

    async fn update_impl(&self, source: Option<&str>) -> anyhow::Result<()> {
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();
        let identity = match source {
            Some(source) => Some(self.get_package_identity(source, None)?),
            None => None,
        };
        let mut matched = false;
        let mut update_sources: Vec<ConfiguredUpdateSource> = Vec::new();

        let global_packages = global_settings.packages.clone().unwrap_or_default();
        let project_packages = project_settings.packages.clone().unwrap_or_default();
        for (packages, scope) in [
            (&global_packages, SettingsScope::User),
            (&project_packages, SettingsScope::Project),
        ] {
            for pkg in packages {
                let source_str = package_source_string(pkg);
                if let Some(identity) = &identity
                    && self.get_package_identity(source_str, Some(installed_to_source_scope(scope)))?
                        != *identity
                {
                    continue;
                }
                matched = true;
                update_sources.push(ConfiguredUpdateSource {
                    source: source_str.to_string(),
                    scope,
                });
            }
        }

        if let Some(source) = source
            && !matched
        {
            let all: Vec<PackageSource> = global_packages
                .iter()
                .chain(project_packages.iter())
                .cloned()
                .collect();
            bail!("{}", self.build_no_matching_package_message(source, &all));
        }

        self.update_configured_sources(update_sources).await
    }

    async fn update_configured_sources(&self, sources: Vec<ConfiguredUpdateSource>) -> anyhow::Result<()> {
        if is_offline_mode_enabled() || sources.is_empty() {
            return Ok(());
        }

        let mut npm_candidates: Vec<NpmUpdateTarget> = Vec::new();
        let mut git_candidates: Vec<GitUpdateTarget> = Vec::new();

        for entry in sources {
            // Pinned npm versions are fixed. Pinned git refs are configured checkout targets,
            // so include them to reconcile an existing clone when the configured ref changes.
            match self.parse_source(&entry.source) {
                ParsedSource::NpmSource(parsed) => {
                    if !parsed.pinned {
                        npm_candidates.push(NpmUpdateTarget {
                            source: entry.source,
                            scope: entry.scope,
                            parsed,
                        });
                    }
                }
                ParsedSource::GitSource(parsed) => git_candidates.push(GitUpdateTarget {
                    source: entry.source,
                    scope: entry.scope,
                    parsed,
                }),
                ParsedSource::LocalSource(_) => {}
            }
        }

        let npm_check_tasks = npm_candidates.into_iter().map(|entry| async move {
            let should_update = self.should_update_npm_source(&entry.parsed, entry.scope).await;
            (entry, should_update)
        });
        let npm_check_results = run_with_concurrency(npm_check_tasks, UPDATE_CHECK_CONCURRENCY).await;
        let mut user_npm_updates = Vec::new();
        let mut project_npm_updates = Vec::new();
        for (entry, should_update) in npm_check_results {
            if !should_update? {
                continue;
            }
            if entry.scope == SettingsScope::User {
                user_npm_updates.push(entry);
            } else {
                project_npm_updates.push(entry);
            }
        }

        let user_task = async {
            if user_npm_updates.is_empty() {
                return Ok(());
            }
            self.update_npm_batch(&user_npm_updates, SettingsScope::User).await
        };
        let project_task = async {
            if project_npm_updates.is_empty() {
                return Ok(());
            }
            self.update_npm_batch(&project_npm_updates, SettingsScope::Project)
                .await
        };
        let git_task = async {
            if git_candidates.is_empty() {
                return Ok(());
            }
            let git_tasks = git_candidates.iter().map(|entry| async move {
                let message = format!("Updating {}...", entry.source);
                self.with_progress(ProgressEventAction::Update, &entry.source, &message, async {
                    self.update_git(&entry.parsed, installed_to_source_scope(entry.scope))
                        .await
                })
                .await
            });
            for result in run_with_concurrency(git_tasks, GIT_UPDATE_CONCURRENCY).await {
                result?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let (user_result, project_result, git_result) =
            futures::join!(user_task, project_task, git_task);
        user_result?;
        project_result?;
        git_result
    }

    async fn should_update_npm_source(
        &self,
        source: &NpmSource,
        scope: InstalledSourceScope,
    ) -> anyhow::Result<bool> {
        let installed_path = self.get_managed_npm_install_path(source, installed_to_source_scope(scope))?;
        let installed_version = if exists(&installed_path) {
            self.get_installed_npm_version(&installed_path)
        } else {
            None
        };
        let Some(installed_version) = installed_version else {
            return Ok(true);
        };

        let spec = if source.version.is_some() {
            &source.spec
        } else {
            &source.name
        };
        match self.get_latest_npm_version(spec, source.range.as_deref()).await {
            Ok(target_version) => {
                // Preserve existing update behavior when version lookup fails.
                Ok(node_semver::gt(&target_version, &installed_version).unwrap_or(true))
            }
            Err(_) => Ok(true),
        }
    }

    async fn update_npm_batch(
        &self,
        sources: &[NpmUpdateTarget],
        scope: InstalledSourceScope,
    ) -> anyhow::Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        let scope_name = scope.as_str();
        let source_label = if sources.len() == 1 {
            sources[0].source.clone()
        } else {
            format!("{scope_name} npm packages")
        };
        let message = if sources.len() == 1 {
            format!("Updating {}...", sources[0].source)
        } else {
            format!("Updating {scope_name} npm packages...")
        };
        let specs: Vec<String> = sources
            .iter()
            .map(|entry| {
                if entry.parsed.version.is_some() {
                    entry.parsed.spec.clone()
                } else {
                    format!("{}@latest", entry.parsed.name)
                }
            })
            .collect();

        self.with_progress(ProgressEventAction::Update, &source_label, &message, async {
            self.install_npm_batch(&specs, scope).await
        })
        .await
    }

    async fn install_npm_batch(&self, specs: &[String], scope: InstalledSourceScope) -> anyhow::Result<()> {
        let install_root = self.get_npm_install_root(installed_to_source_scope(scope), false)?;
        self.ensure_npm_project(&install_root)?;
        self.npm_install_specs(specs, &install_root).await
    }

    pub async fn check_for_available_updates(&self) -> anyhow::Result<Vec<PackageUpdate>> {
        if is_offline_mode_enabled() {
            return Ok(Vec::new());
        }

        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in project_settings.packages.unwrap_or_default() {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in global_settings.packages.unwrap_or_default() {
            all_packages.push((pkg, SourceScope::User));
        }

        let package_sources = self.dedupe_packages(all_packages)?;
        let checks = package_sources
            .into_iter()
            .filter(|(_, scope)| *scope != SourceScope::Temporary)
            .map(|(pkg, scope)| async move {
                let source = package_source_string(&pkg).to_string();
                let settings_scope = if scope == SourceScope::Project {
                    SettingsScope::Project
                } else {
                    SettingsScope::User
                };
                match self.parse_source(&source) {
                    ParsedSource::LocalSource(_) => Ok(None),
                    ParsedSource::NpmSource(parsed) => {
                        if parsed.pinned {
                            return Ok(None);
                        }
                        let installed_path = self.get_npm_install_path(&parsed, scope)?;
                        if !exists(&installed_path) {
                            return Ok(None);
                        }
                        if !self.npm_has_available_update(&parsed, &installed_path).await {
                            return Ok(None);
                        }
                        Ok(Some(PackageUpdate {
                            source,
                            display_name: parsed.name.clone(),
                            r#type: PackageUpdateType::Npm,
                            scope: settings_scope,
                        }))
                    }
                    ParsedSource::GitSource(parsed) => {
                        if parsed.pinned {
                            return Ok(None);
                        }
                        let installed_path = self.get_git_install_path(&parsed, scope)?;
                        if !exists(&installed_path) {
                            return Ok(None);
                        }
                        if !self.git_has_available_update(&installed_path).await {
                            return Ok(None);
                        }
                        Ok::<_, anyhow::Error>(Some(PackageUpdate {
                            source,
                            display_name: format!("{}/{}", parsed.host, parsed.path),
                            r#type: PackageUpdateType::Git,
                            scope: settings_scope,
                        }))
                    }
                }
            });

        let results = run_with_concurrency(checks, UPDATE_CHECK_CONCURRENCY).await;
        let mut updates = Vec::new();
        for result in results {
            if let Some(update) = result? {
                updates.push(update);
            }
        }
        Ok(updates)
    }

    async fn resolve_package_sources(
        &self,
        sources: &[(PackageSource, SourceScope)],
        accumulator: &mut ResourceAccumulator,
        on_missing: Option<MissingSourceHandler>,
    ) -> anyhow::Result<Vec<PackageInstallFailure>> {
        let mut install_failures = Vec::new();
        for (pkg, scope) in sources {
            let scope = *scope;
            let source_str = package_source_string(pkg).to_string();
            let filter = package_filter(pkg);
            let delta_base = self.find_autoload_delta_base(pkg, scope, sources)?;
            let resolved_source = delta_base
                .as_ref()
                .map(|(s, _)| s.clone())
                .unwrap_or_else(|| source_str.clone());
            let resolved_scope = delta_base.as_ref().map(|(_, s)| *s).unwrap_or(scope);
            let parsed = self.parse_source(&resolved_source);
            let mut metadata = PathMetadata {
                source: source_str.clone(),
                scope,
                origin: SourceOrigin::Package,
                base_dir: None,
                package_root: None,
            };

            if let ParsedSource::LocalSource(local) = &parsed {
                let base_dir = self.get_base_dir_for_scope(resolved_scope)?;
                self.resolve_local_extension_source(local, accumulator, filter.as_ref(), metadata, &base_dir);
                continue;
            }

            match &parsed {
                ParsedSource::NpmSource(npm) => {
                    let mut installed_path = self.get_npm_install_path(npm, resolved_scope)?;
                    let needs_install = !exists(&installed_path)
                        || !self.installed_npm_matches_configured_version(npm, &installed_path);
                    if needs_install {
                        if !self
                            .install_missing_source(
                                &parsed,
                                resolved_scope,
                                &resolved_source,
                                on_missing.as_ref(),
                                &mut install_failures,
                            )
                            .await?
                        {
                            continue;
                        }
                        installed_path = self.get_npm_install_path(npm, resolved_scope)?;
                    }
                    metadata.base_dir = Some(installed_path.clone());
                    metadata.package_root = Some(installed_path.clone());
                    self.collect_package_resources(&installed_path, accumulator, filter.as_ref(), &metadata);
                }
                ParsedSource::GitSource(git) => {
                    let installed_path = self.get_git_install_path(git, resolved_scope)?;
                    if !exists(&installed_path) {
                        if !self
                            .install_missing_source(
                                &parsed,
                                resolved_scope,
                                &resolved_source,
                                on_missing.as_ref(),
                                &mut install_failures,
                            )
                            .await?
                        {
                            continue;
                        }
                    } else if resolved_scope == SourceScope::Temporary
                        && !git.pinned
                        && !is_offline_mode_enabled()
                    {
                        self.refresh_temporary_git_source(git, &resolved_source).await;
                    }
                    metadata.base_dir = Some(installed_path.clone());
                    metadata.package_root = Some(installed_path.clone());
                    self.collect_package_resources(&installed_path, accumulator, filter.as_ref(), &metadata);
                }
                ParsedSource::LocalSource(_) => {}
            }
        }
        Ok(install_failures)
    }

    /// Only installation failures are recoverable. Caller policy errors still propagate.
    async fn install_missing_source(
        &self,
        parsed: &ParsedSource,
        scope: SourceScope,
        source: &str,
        on_missing: Option<&MissingSourceHandler>,
        install_failures: &mut Vec<PackageInstallFailure>,
    ) -> anyhow::Result<bool> {
        if is_offline_mode_enabled() {
            return Ok(false);
        }
        if let Some(on_missing) = on_missing {
            match on_missing(source.to_string()).await? {
                MissingSourceAction::Skip => return Ok(false),
                MissingSourceAction::Error => bail!("Missing source: {source}"),
                MissingSourceAction::Install => {}
            }
        }
        match self.install_parsed_source(parsed, scope).await {
            Ok(()) => Ok(true),
            Err(error) => {
                install_failures.push(PackageInstallFailure {
                    source: source.to_string(),
                    scope,
                    message: error.to_string(),
                });
                Ok(false)
            }
        }
    }

    fn find_autoload_delta_base(
        &self,
        pkg: &PackageSource,
        scope: SourceScope,
        sources: &[(PackageSource, SourceScope)],
    ) -> anyhow::Result<Option<(String, SourceScope)>> {
        let PackageSource::PackageSourceVariant2(obj) = pkg else {
            return Ok(None);
        };
        if scope != SourceScope::Project || obj.autoload != Some(false) {
            return Ok(None);
        }
        let identity = self.get_package_identity(&obj.source, Some(scope))?;
        for (entry_pkg, entry_scope) in sources {
            if *entry_scope == SourceScope::User
                && self.get_package_identity(package_source_string(entry_pkg), Some(SourceScope::User))?
                    == identity
            {
                return Ok(Some((
                    package_source_string(entry_pkg).to_string(),
                    SourceScope::User,
                )));
            }
        }
        Ok(None)
    }

    fn resolve_local_extension_source(
        &self,
        source: &LocalSource,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        mut metadata: PathMetadata,
        base_dir: &str,
    ) {
        let resolved = self.resolve_path_from_base(&source.path, base_dir);
        let Ok(stats) = std::fs::metadata(&resolved) else {
            return;
        };
        if stats.is_file() {
            metadata.base_dir = Some(node_dirname(&resolved));
            Self::add_resource(&mut accumulator.extensions, &resolved, &metadata, true);
            return;
        }
        if stats.is_dir() {
            metadata.base_dir = Some(resolved.clone());
            metadata.package_root = Some(resolved.clone());
            let resources = self.collect_package_resources(&resolved, accumulator, filter, &metadata);
            if !resources {
                Self::add_resource(&mut accumulator.extensions, &resolved, &metadata, true);
            }
        }
    }

    async fn install_parsed_source(&self, parsed: &ParsedSource, scope: SourceScope) -> anyhow::Result<()> {
        match parsed {
            ParsedSource::NpmSource(npm) => {
                self.install_npm(npm, scope, scope == SourceScope::Temporary).await
            }
            ParsedSource::GitSource(git) => self.install_git(git, scope).await,
            ParsedSource::LocalSource(_) => Ok(()),
        }
    }

    pub(crate) fn get_source_match_key_for_input(&self, source: &str) -> String {
        match self.parse_source(source) {
            ParsedSource::NpmSource(parsed) => format!("npm:{}", parsed.name),
            ParsedSource::GitSource(parsed) => format!("git:{}/{}", parsed.host, parsed.path),
            ParsedSource::LocalSource(parsed) => format!("local:{}", self.resolve_path(&parsed.path)),
        }
    }

    fn get_source_match_key_for_settings(&self, source: &str, scope: SourceScope) -> anyhow::Result<String> {
        Ok(match self.parse_source(source) {
            ParsedSource::NpmSource(parsed) => format!("npm:{}", parsed.name),
            ParsedSource::GitSource(parsed) => format!("git:{}/{}", parsed.host, parsed.path),
            ParsedSource::LocalSource(parsed) => {
                let base_dir = self.get_base_dir_for_scope(scope)?;
                format!("local:{}", self.resolve_path_from_base(&parsed.path, &base_dir))
            }
        })
    }

    pub(crate) fn build_no_matching_package_message(&self, source: &str, configured_packages: &[PackageSource]) -> String {
        match self.find_suggested_configured_source(source, configured_packages) {
            Some(suggestion) => {
                format!("No matching package found for {source}. Did you mean {suggestion}?")
            }
            None => format!("No matching package found for {source}"),
        }
    }

    fn find_suggested_configured_source(
        &self,
        source: &str,
        configured_packages: &[PackageSource],
    ) -> Option<String> {
        let trimmed_source = source.trim();
        for pkg in configured_packages {
            let source_str = package_source_string(pkg);
            match self.parse_source(source_str) {
                ParsedSource::NpmSource(parsed) => {
                    if trimmed_source == parsed.name || trimmed_source == parsed.spec {
                        return Some(source_str.to_string());
                    }
                }
                ParsedSource::GitSource(parsed) => {
                    let shorthand = format!("{}/{}", parsed.host, parsed.path);
                    let shorthand_with_ref = parsed.r#ref.as_ref().map(|r| format!("{shorthand}@{r}"));
                    if trimmed_source == shorthand
                        || shorthand_with_ref.as_deref() == Some(trimmed_source)
                    {
                        return Some(source_str.to_string());
                    }
                }
                ParsedSource::LocalSource(_) => {}
            }
        }
        None
    }

    fn package_sources_match(
        &self,
        existing: &PackageSource,
        input_source: &str,
        scope: SourceScope,
    ) -> anyhow::Result<bool> {
        let left = self.get_source_match_key_for_settings(package_source_string(existing), scope)?;
        let right = self.get_source_match_key_for_input(input_source);
        Ok(left == right)
    }

    pub(crate) fn normalize_package_source_for_settings(&self, source: &str, scope: SourceScope) -> anyhow::Result<String> {
        let ParsedSource::LocalSource(parsed) = self.parse_source(source) else {
            return Ok(source.to_string());
        };
        let base_dir = self.get_base_dir_for_scope(scope)?;
        let resolved = self.resolve_path(&parsed.path);
        let rel = node_relative(&base_dir, &resolved);
        Ok(if rel.is_empty() { ".".to_string() } else { rel })
    }

    pub(crate) fn parse_source(&self, source: &str) -> ParsedSource {
        if let Some(rest) = source.strip_prefix("npm:") {
            let spec = rest.trim().to_string();
            let (name, version) = Self::parse_npm_spec(&spec);
            return ParsedSource::NpmSource(NpmSource {
                r#type: "npm".to_string(),
                range: get_npm_version_range(version.as_deref()),
                pinned: is_exact_npm_version(version.as_deref()),
                spec,
                name,
                version,
            });
        }

        if is_local_path(source) {
            return ParsedSource::LocalSource(LocalSource {
                r#type: "local".to_string(),
                path: source.to_string(),
            });
        }

        // Try parsing as git URL
        if let Some(git) = parse_git_url(source) {
            return ParsedSource::GitSource(git);
        }

        ParsedSource::LocalSource(LocalSource {
            r#type: "local".to_string(),
            path: source.to_string(),
        })
    }

    fn installed_npm_matches_configured_version(&self, source: &NpmSource, installed_path: &str) -> bool {
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        match &source.range {
            Some(range) => node_semver::satisfies(&installed_version, range),
            None => true,
        }
    }

    async fn npm_has_available_update(&self, source: &NpmSource, installed_path: &str) -> bool {
        if is_offline_mode_enabled() {
            return false;
        }
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        let spec = if source.version.is_some() {
            &source.spec
        } else {
            &source.name
        };
        match self.get_latest_npm_version(spec, source.range.as_deref()).await {
            Ok(target_version) => node_semver::gt(&target_version, &installed_version).unwrap_or(false),
            Err(_) => false,
        }
    }

    fn get_installed_npm_version(&self, installed_path: &str) -> Option<String> {
        let package_json_path = node_join(&[installed_path, "package.json"]);
        let content = std::fs::read_to_string(&package_json_path).ok()?;
        let pkg: serde_json::Value = serde_json::from_str(&strip_bom(&content)).ok()?;
        pkg.get("version")?.as_str().map(str::to_string)
    }

    pub(crate) async fn get_latest_npm_version(
        &self,
        package_spec: &str,
        range: Option<&str>,
    ) -> anyhow::Result<String> {
        if self.use_builtin_npm() {
            let install_root = self.agent_dir.clone();
            let client = self.registry_client(&install_root);
            return client.latest_version(package_spec, range).await;
        }
        let npm_command = self.get_npm_command()?;
        let mut args = npm_command.args.clone();
        args.extend(strs(&["view", package_spec, "version", "--json"]));
        let stdout = self
            .runner
            .run_capture(
                &npm_command.command,
                &args,
                &CaptureOptions {
                    cwd: Some(self.cwd.clone()),
                    timeout_ms: Some(NETWORK_TIMEOUT_MS),
                    env: None,
                },
            )
            .await?;
        let raw = stdout.trim();
        if raw.is_empty() {
            bail!("Empty response from npm view");
        }
        let parsed: serde_json::Value = serde_json::from_str(raw)?;
        if let Some(version) = parsed.as_str() {
            return Ok(version.to_string());
        }
        if let Some(items) = parsed.as_array() {
            let versions: Vec<&str> = items
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .collect();
            let latest = match range {
                Some(range) => node_semver::max_satisfying(versions.iter().copied(), range),
                None => node_semver::max_version(versions.iter().copied()),
            };
            if let Some(latest) = latest {
                return Ok(latest);
            }
        }
        bail!("Unexpected response from npm view")
    }

    async fn git_capture(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
        self.runner
            .run_capture(
                "git",
                &strs(args),
                &CaptureOptions {
                    cwd: Some(cwd.to_string()),
                    timeout_ms: Some(NETWORK_TIMEOUT_MS),
                    env: None,
                },
            )
            .await
    }

    async fn git_has_available_update(&self, installed_path: &str) -> bool {
        if is_offline_mode_enabled() {
            return false;
        }
        let result: anyhow::Result<bool> = async {
            let local_head = self.git_capture(installed_path, &["rev-parse", "HEAD"]).await?;
            let remote_head = self.get_remote_git_head(installed_path).await?;
            Ok(local_head.trim() != remote_head.trim())
        }
        .await;
        result.unwrap_or(false)
    }

    async fn get_remote_git_head(&self, installed_path: &str) -> anyhow::Result<String> {
        if let Some(upstream_ref) = self.get_git_upstream_ref(installed_path).await {
            let remote_head = self
                .run_git_remote_command(installed_path, &["ls-remote", "origin", &upstream_ref])
                .await?;
            for line in remote_head.lines() {
                if let Some(hash) = parse_ls_remote_line(line, None) {
                    return Ok(hash);
                }
            }
        }

        let remote_head = self
            .run_git_remote_command(installed_path, &["ls-remote", "origin", "HEAD"])
            .await?;
        for line in remote_head.lines() {
            if let Some(hash) = parse_ls_remote_line(line, Some("HEAD")) {
                return Ok(hash);
            }
        }
        bail!("Failed to determine remote HEAD")
    }

    pub(crate) async fn get_local_git_update_target(&self, installed_path: &str) -> anyhow::Result<GitUpdateTargetRef> {
        let upstream_result: anyhow::Result<GitUpdateTargetRef> = async {
            let upstream = self
                .git_capture(installed_path, &["rev-parse", "--abbrev-ref", "@{upstream}"])
                .await?;
            let trimmed_upstream = upstream.trim();
            let Some(branch) = trimmed_upstream.strip_prefix("origin/") else {
                bail!("Unsupported upstream remote: {trimmed_upstream}");
            };
            if branch.is_empty() {
                bail!("Missing upstream branch name");
            }
            let head = self.git_capture(installed_path, &["rev-parse", "@{upstream}"]).await?;
            Ok(GitUpdateTargetRef {
                r#ref: "@{upstream}".to_string(),
                head,
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ],
            })
        }
        .await;
        if let Ok(target) = upstream_result {
            return Ok(target);
        }

        let _ = self
            .runner
            .run("git", &strs(&["remote", "set-head", "origin", "-a"]), Some(installed_path))
            .await;
        let head = self.git_capture(installed_path, &["rev-parse", "origin/HEAD"]).await?;
        let origin_head_ref = self
            .git_capture(installed_path, &["symbolic-ref", "refs/remotes/origin/HEAD"])
            .await
            .unwrap_or_default();
        let trimmed = origin_head_ref.trim();
        let branch = trimmed.strip_prefix("refs/remotes/origin/").unwrap_or(trimmed);
        if !branch.is_empty() {
            return Ok(GitUpdateTargetRef {
                r#ref: "origin/HEAD".to_string(),
                head,
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ],
            });
        }
        Ok(GitUpdateTargetRef {
            r#ref: "origin/HEAD".to_string(),
            head,
            fetch_args: strs(&[
                "fetch",
                "--prune",
                "--no-tags",
                "origin",
                "+HEAD:refs/remotes/origin/HEAD",
            ]),
        })
    }

    async fn get_git_upstream_ref(&self, installed_path: &str) -> Option<String> {
        let upstream = self
            .git_capture(installed_path, &["rev-parse", "--abbrev-ref", "@{upstream}"])
            .await
            .ok()?;
        let branch = upstream.trim().strip_prefix("origin/")?;
        (!branch.is_empty()).then(|| format!("refs/heads/{branch}"))
    }

    async fn run_git_remote_command(&self, installed_path: &str, args: &[&str]) -> anyhow::Result<String> {
        self.runner
            .run_capture(
                "git",
                &strs(args),
                &CaptureOptions {
                    cwd: Some(installed_path.to_string()),
                    timeout_ms: Some(NETWORK_TIMEOUT_MS),
                    env: Some(vec![("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())]),
                },
            )
            .await
    }

    /// Get a unique identity for a package, ignoring version/ref.
    /// Used to detect when the same package is in both global and project settings.
    /// For git packages, uses normalized host/path to ensure SSH and HTTPS URLs
    /// for the same repository are treated as identical.
    pub(crate) fn get_package_identity(&self, source: &str, scope: Option<SourceScope>) -> anyhow::Result<String> {
        Ok(match self.parse_source(source) {
            ParsedSource::NpmSource(parsed) => format!("npm:{}", parsed.name),
            // Use host/path for identity to normalize SSH and HTTPS
            ParsedSource::GitSource(parsed) => format!("git:{}/{}", parsed.host, parsed.path),
            ParsedSource::LocalSource(parsed) => match scope {
                Some(scope) => {
                    let base_dir = self.get_base_dir_for_scope(scope)?;
                    format!("local:{}", self.resolve_path_from_base(&parsed.path, &base_dir))
                }
                None => format!("local:{}", self.resolve_path(&parsed.path)),
            },
        })
    }

    /// Dedupe packages: if same package identity appears in both global and project,
    /// keep only the project one (project wins). A project entry with autoload=false
    /// is a delta over the global entry, so both are kept (delta first).
    fn dedupe_packages(
        &self,
        packages: Vec<(PackageSource, SourceScope)>,
    ) -> anyhow::Result<Vec<(PackageSource, SourceScope)>> {
        let mut result: Vec<(PackageSource, SourceScope)> = Vec::new();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for entry in packages {
            let identity = self.get_package_identity(package_source_string(&entry.0), Some(entry.1))?;
            let Some(&index) = seen.get(&identity) else {
                seen.insert(identity, result.len());
                result.push(entry);
                continue;
            };
            let existing = &result[index];
            if existing.1 == SourceScope::Project && entry.1 == SourceScope::User {
                if let PackageSource::PackageSourceVariant2(obj) = &existing.0
                    && obj.autoload == Some(false)
                {
                    result.push(entry);
                }
            } else if entry.1 == SourceScope::Project {
                result[index] = entry;
            }
        }
        Ok(result)
    }

    /// `spec.match(/^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$/)`: the name and the version after the first
    /// `@` that follows the name. Without a match (`foo@`, `@`, a version with a line break) the
    /// whole spec is the name.
    fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
        let rest = spec.strip_prefix('@').unwrap_or(spec);
        let scope_len = spec.len() - rest.len();
        let whole = || (spec.to_string(), None);
        match rest.find('@') {
            None if !rest.is_empty() => whole(),
            Some(index) if index > 0 => {
                let version = &rest[index + 1..];
                // `.` does not match JS line terminators.
                if version.is_empty() || version.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                    return whole();
                }
                (spec[..scope_len + index].to_string(), Some(version.to_string()))
            }
            _ => whole(),
        }
    }

    fn assert_project_trusted_for_scope(&self, scope: SourceScope) -> anyhow::Result<()> {
        if scope == SourceScope::Project && !self.settings_manager.is_project_trusted() {
            bail!("Project is not trusted; refusing to access project package storage");
        }
        Ok(())
    }

    fn get_npm_command(&self) -> anyhow::Result<NpmCommand> {
        let configured = self.settings_manager.get_npm_command();
        let Some(configured) = configured.filter(|c| !c.is_empty()) else {
            return Ok(NpmCommand {
                command: "npm".to_string(),
                args: Vec::new(),
            });
        };
        let command = configured[0].clone();
        if command.is_empty() {
            bail!("Invalid npmCommand: first array entry must be a non-empty command");
        }
        Ok(NpmCommand {
            command,
            args: configured[1..].to_vec(),
        })
    }

    /// Use the built-in registry client instead of an npm executable.
    fn use_builtin_npm(&self) -> bool {
        if self
            .settings_manager
            .get_npm_command()
            .is_some_and(|c| !c.is_empty())
        {
            return false;
        }
        is_builtin_npm_forced() || !self.runner.command_exists("npm")
    }

    fn registry_client(&self, install_root: &str) -> npm_registry::NpmRegistryClient {
        npm_registry::NpmRegistryClient::new(&[PathBuf::from(&self.cwd), PathBuf::from(install_root)])
    }

    pub(crate) fn get_package_manager_name(&self) -> anyhow::Result<String> {
        let npm_command = self.get_npm_command()?;
        let normalize_command_name = |command: &str| -> String {
            let base = node_basename(command);
            let lower = base.to_lowercase();
            if lower.ends_with(".cmd") || lower.ends_with(".exe") {
                base[..base.len() - 4].to_string()
            } else {
                base
            }
        };
        let supported = ["npm", "pnpm", "bun"];
        let direct_command = normalize_command_name(&npm_command.command);
        if let Some(separator_index) = npm_command.args.iter().rposition(|a| a == "--") {
            return Ok(match npm_command.args.get(separator_index + 1) {
                Some(wrapped) if !wrapped.is_empty() => normalize_command_name(wrapped),
                _ => direct_command,
            });
        }
        if supported.contains(&direct_command.as_str()) {
            return Ok(direct_command);
        }

        let mut wrapped: Vec<String> = Vec::new();
        for arg in &npm_command.args {
            let name = normalize_command_name(arg);
            if supported.contains(&name.as_str()) && !wrapped.contains(&name) {
                wrapped.push(name);
            }
        }
        if wrapped.len() > 1 {
            bail!("Ambiguous npmCommand package managers: {}", wrapped.join(", "));
        }
        Ok(wrapped.into_iter().next().unwrap_or(direct_command))
    }

    async fn run_npm_command(&self, args: Vec<String>, cwd: Option<&str>) -> anyhow::Result<()> {
        let npm_command = self.get_npm_command()?;
        let mut full_args = npm_command.args.clone();
        full_args.extend(args);
        self.runner.run(&npm_command.command, &full_args, cwd).await
    }

    pub(crate) fn get_git_dependency_install_args(&self) -> anyhow::Result<Vec<String>> {
        Ok(match self.get_package_manager_name()?.as_str() {
            "bun" => strs(&["install", "--omit=dev", "--omit=peer"]),
            "pnpm" => strs(&[
                "install",
                "--prod",
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]),
            "npm" => strs(&["install", "--omit=dev", "--legacy-peer-deps"]),
            _ => strs(&["install"]),
        })
    }

    /// Install a git checkout's dependencies (`npm install --omit=dev` or equivalent).
    async fn install_git_dependencies(&self, target_dir: &str) -> anyhow::Result<()> {
        if self.use_builtin_npm() {
            return self
                .registry_client(target_dir)
                .install_dependencies(Path::new(target_dir))
                .await;
        }
        self.run_npm_command(self.get_git_dependency_install_args()?, Some(target_dir))
            .await
    }

    fn run_npm_command_sync(&self, args: &[&str]) -> anyhow::Result<String> {
        let npm_command = self.get_npm_command()?;
        let mut full_args = npm_command.args.clone();
        full_args.extend(strs(args));
        self.runner.run_sync(&npm_command.command, &full_args)
    }

    pub(crate) fn get_npm_install_args(&self, specs: &[String], install_root: &str) -> anyhow::Result<Vec<String>> {
        let package_manager_name = self.get_package_manager_name()?;
        // Extension packages run inside pi and resolve pi APIs through loader aliases/virtual modules.
        // Disable peer dependency resolution for managed installs (npm's --legacy-peer-deps, and
        // equivalent bun/pnpm settings) so package managers do not install or solve host-provided
        // @earendil-works/pi-* peers. Stale auto-installed pi peers can otherwise block updates.
        let mut args = vec!["install".to_string()];
        args.extend(specs.iter().cloned());
        if package_manager_name == "bun" {
            args.extend(strs(&["--cwd", install_root, "--omit=peer"]));
            return Ok(args);
        }
        if package_manager_name == "pnpm" {
            args.extend(strs(&[
                "--prefix",
                install_root,
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]));
            return Ok(args);
        }
        args.extend(strs(&["--prefix", install_root, "--legacy-peer-deps"]));
        Ok(args)
    }

    async fn npm_install_specs(&self, specs: &[String], install_root: &str) -> anyhow::Result<()> {
        if self.use_builtin_npm() {
            return self
                .registry_client(install_root)
                .install_packages(Path::new(install_root), specs)
                .await;
        }
        self.run_npm_command(self.get_npm_install_args(specs, install_root)?, None)
            .await
    }

    async fn install_npm(&self, source: &NpmSource, scope: SourceScope, temporary: bool) -> anyhow::Result<()> {
        let install_root = self.get_npm_install_root(scope, temporary)?;
        self.ensure_npm_project(&install_root)?;
        self.npm_install_specs(std::slice::from_ref(&source.spec), &install_root)
            .await
    }

    async fn uninstall_npm(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<()> {
        let install_root = self.get_npm_install_root(scope, false)?;
        if !exists(&install_root) {
            return Ok(());
        }
        if self.use_builtin_npm() {
            return self
                .registry_client(&install_root)
                .uninstall(Path::new(&install_root), &source.name)
                .await;
        }
        let package_manager_name = self.get_package_manager_name()?;
        if package_manager_name == "bun" {
            return self
                .run_npm_command(strs(&["uninstall", &source.name, "--cwd", &install_root]), None)
                .await;
        }
        let mut args = strs(&["uninstall", &source.name, "--prefix", &install_root]);
        if package_manager_name != "pnpm" {
            args.push("--legacy-peer-deps".to_string());
        }
        self.run_npm_command(args, None).await
    }

    async fn install_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if exists(&target_dir) {
            if let Some(r) = &source.r#ref {
                return self
                    .ensure_git_ref(&target_dir, strs(&["fetch", "origin", r]), "FETCH_HEAD")
                    .await;
            }
            let target = self.get_local_git_update_target(&target_dir).await?;
            return self
                .ensure_git_ref(&target_dir, target.fetch_args, &target.r#ref)
                .await;
        }
        let git_root = self.get_git_install_root(scope)?;
        if let Some(git_root) = &git_root {
            self.ensure_git_ignore(git_root)?;
        }
        std::fs::create_dir_all(node_dirname(&target_dir))?;
        let _ = std::fs::remove_file(self.get_git_update_marker_path(&target_dir));

        let result: anyhow::Result<()> = async {
            self.runner
                .run("git", &strs(&["clone", &source.repo, &target_dir]), None)
                .await?;
            if let Some(r) = &source.r#ref {
                self.runner
                    .run("git", &strs(&["checkout", r]), Some(&target_dir))
                    .await?;
            }
            if exists(&node_join(&[&target_dir, "package.json"])) {
                self.install_git_dependencies(&target_dir).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&target_dir);
            self.prune_empty_git_parents(&target_dir, git_root.as_deref());
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn update_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if !exists(&target_dir) {
            return self.install_git(source, scope).await;
        }
        if let Some(r) = &source.r#ref {
            return self
                .ensure_git_ref(&target_dir, strs(&["fetch", "origin", r]), "FETCH_HEAD")
                .await;
        }
        let target = self.get_local_git_update_target(&target_dir).await?;
        self.ensure_git_ref(&target_dir, target.fetch_args, &target.r#ref)
            .await
    }

    fn has_missing_git_dependencies(&self, target_dir: &str) -> bool {
        let package_json_path = node_join(&[target_dir, "package.json"]);
        let Ok(content) = std::fs::read_to_string(&package_json_path) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&strip_bom(&content)) else {
            return false;
        };
        let Some(dependencies) = manifest.get("dependencies").and_then(|d| d.as_object()) else {
            return false;
        };
        let node_modules_dir = node_resolve(&[target_dir, "node_modules"]);
        dependencies.keys().any(|name| {
            let dependency_path = node_resolve(&[&node_modules_dir, name]);
            if !dependency_path.starts_with(&format!("{node_modules_dir}{MAIN_SEPARATOR}")) {
                return false;
            }
            !exists(&dependency_path)
        })
    }

    async fn repair_missing_git_dependencies(&self, target_dir: &str) -> anyhow::Result<()> {
        if !self.has_missing_git_dependencies(target_dir) {
            return Ok(());
        }
        self.install_git_dependencies(target_dir).await
    }

    fn get_git_update_marker_path(&self, target_dir: &str) -> String {
        node_join(&[
            &node_dirname(target_dir),
            &format!(".{}.pi-update-incomplete", node_basename(target_dir)),
        ])
    }

    async fn clean_and_install_git_dependencies(&self, target_dir: &str, marker_path: &str) -> anyhow::Result<()> {
        // Clean untracked files (extensions should be pristine). If this fails after
        // deleting dependencies, repair them so the existing extension still loads.
        if let Err(error) = self
            .runner
            .run("git", &strs(&["clean", "-fdx"]), Some(target_dir))
            .await
        {
            let _ = self.repair_missing_git_dependencies(target_dir).await;
            return Err(error);
        }

        if exists(&node_join(&[target_dir, "package.json"])) {
            self.install_git_dependencies(target_dir).await?;
        }
        let _ = std::fs::remove_file(marker_path);
        Ok(())
    }

    async fn ensure_git_ref(&self, target_dir: &str, fetch_args: Vec<String>, r#ref: &str) -> anyhow::Result<()> {
        // Fetch only the ref we will reset to, avoiding unrelated branch/tag noise.
        self.runner.run("git", &fetch_args, Some(target_dir)).await?;

        let local_head = self.git_capture(target_dir, &["rev-parse", "HEAD"]).await?;
        let commit_ref = format!("{ref}^{{commit}}");
        let target_head = self.git_capture(target_dir, &["rev-parse", &commit_ref]).await?;
        let marker_path = self.get_git_update_marker_path(target_dir);
        if local_head.trim() == target_head.trim() {
            if exists(&marker_path) {
                self.clean_and_install_git_dependencies(target_dir, &marker_path)
                    .await?;
            } else {
                self.repair_missing_git_dependencies(target_dir).await?;
            }
            return Ok(());
        }

        std::fs::write(&marker_path, "")?;
        self.runner
            .run("git", &strs(&["reset", "--hard", &commit_ref]), Some(target_dir))
            .await?;
        self.clean_and_install_git_dependencies(target_dir, &marker_path)
            .await
    }

    async fn refresh_temporary_git_source(&self, source: &GitSource, source_str: &str) {
        if is_offline_mode_enabled() {
            return;
        }
        // Keep cached temporary checkout if refresh fails.
        let _ = self
            .with_progress(
                ProgressEventAction::Pull,
                source_str,
                &format!("Refreshing {source_str}..."),
                self.update_git(source, SourceScope::Temporary),
            )
            .await;
    }

    fn remove_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target_dir = self.get_git_install_path(source, scope)?;
        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_file(self.get_git_update_marker_path(&target_dir));
        self.prune_empty_git_parents(&target_dir, self.get_git_install_root(scope)?.as_deref());
        Ok(())
    }

    fn prune_empty_git_parents(&self, target_dir: &str, install_root: Option<&str>) {
        let Some(install_root) = install_root else {
            return;
        };
        let resolved_root = node_resolve(&[install_root]);
        let mut current = node_dirname(target_dir);
        while current.starts_with(&resolved_root) && current != resolved_root {
            if !exists(&current) {
                current = node_dirname(&current);
                continue;
            }
            let Ok(mut entries) = std::fs::read_dir(&current) else {
                break;
            };
            if entries.next().is_some() {
                break;
            }
            if std::fs::remove_dir_all(&current).is_err() {
                break;
            }
            current = node_dirname(&current);
        }
    }

    fn ensure_npm_project(&self, install_root: &str) -> anyhow::Result<()> {
        if !exists(install_root) {
            std::fs::create_dir_all(install_root)?;
        }
        mark_path_ignored_by_cloud_sync(install_root);
        self.ensure_git_ignore(install_root)?;
        let package_json_path = node_join(&[install_root, "package.json"]);
        if !exists(&package_json_path) {
            let pkg_json = serde_json::json!({ "name": "pi-extensions", "private": true });
            std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg_json)?)?;
        }
        Ok(())
    }

    fn ensure_git_ignore(&self, dir: &str) -> anyhow::Result<()> {
        if !exists(dir) {
            std::fs::create_dir_all(dir)?;
        }
        let ignore_path = node_join(&[dir, ".gitignore"]);
        if !exists(&ignore_path) {
            std::fs::write(&ignore_path, "*\n!.gitignore\n")?;
        }
        Ok(())
    }

    fn get_npm_install_root(&self, scope: SourceScope, temporary: bool) -> anyhow::Result<String> {
        if temporary {
            return self.get_temporary_dir("npm", None, None);
        }
        if scope == SourceScope::Project {
            self.assert_project_trusted_for_scope(scope)?;
            return Ok(node_join(&[&self.cwd, CONFIG_DIR_NAME, "npm"]));
        }
        Ok(node_join(&[&self.agent_dir, "npm"]))
    }

    pub(crate) fn get_global_npm_root(&self) -> anyhow::Result<String> {
        let npm_command = self.get_npm_command()?;
        let mut key_parts = vec![npm_command.command.clone()];
        key_parts.extend(npm_command.args.iter().cloned());
        let command_key = key_parts.join("\0");
        if let Some((key, root)) = self.global_npm_root.lock().as_ref()
            && *key == command_key
        {
            return Ok(root.clone());
        }
        if self.use_builtin_npm() {
            bail!("npm is not available");
        }
        let root = if self.get_package_manager_name()? == "bun" {
            let bin_dir = self.run_npm_command_sync(&["pm", "bin", "-g"])?;
            node_join(&[&node_dirname(bin_dir.trim()), "install", "global", "node_modules"])
        } else {
            self.run_npm_command_sync(&["root", "-g"])?.trim().to_string()
        };
        *self.global_npm_root.lock() = Some((command_key, root.clone()));
        Ok(root)
    }

    fn get_pnpm_global_package_path(&self, package_name: &str) -> anyhow::Result<Option<String>> {
        if self.get_package_manager_name()? != "pnpm" {
            return Ok(None);
        }
        let output = self.run_npm_command_sync(&["list", "-g", "--depth", "0", "--json"])?;
        let entries: serde_json::Value = serde_json::from_str(&output)?;
        let Some(entries) = entries.as_array() else {
            bail!("Unexpected pnpm list output");
        };
        for entry in entries {
            if let Some(path) = entry
                .get("dependencies")
                .and_then(|d| d.get(package_name))
                .and_then(|p| p.get("path"))
                .and_then(|p| p.as_str())
                && !path.is_empty()
            {
                return Ok(Some(path.to_string()));
            }
        }
        Ok(None)
    }

    fn get_managed_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<String> {
        if scope == SourceScope::Temporary {
            return Ok(node_join(&[
                &self.get_temporary_dir("npm", None, None)?,
                "node_modules",
                &source.name,
            ]));
        }
        if scope == SourceScope::Project {
            self.assert_project_trusted_for_scope(scope)?;
            return Ok(node_join(&[
                &self.cwd,
                CONFIG_DIR_NAME,
                "npm",
                "node_modules",
                &source.name,
            ]));
        }
        Ok(node_join(&[&self.agent_dir, "npm", "node_modules", &source.name]))
    }

    fn get_legacy_global_npm_install_path(&self, source: &NpmSource) -> Option<String> {
        if self.use_builtin_npm() {
            return None;
        }
        let result: anyhow::Result<String> = (|| {
            if let Some(path) = self.get_pnpm_global_package_path(&source.name)? {
                return Ok(path);
            }
            Ok(node_join(&[&self.get_global_npm_root()?, &source.name]))
        })();
        result.ok()
    }

    pub(crate) fn get_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> anyhow::Result<String> {
        let managed_path = self.get_managed_npm_install_path(source, scope)?;
        if scope != SourceScope::User || exists(&managed_path) {
            return Ok(managed_path);
        }
        match self.get_legacy_global_npm_install_path(source) {
            Some(legacy_path) if exists(&legacy_path) => Ok(legacy_path),
            _ => Ok(managed_path),
        }
    }

    pub(crate) fn get_git_install_path(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<String> {
        if scope == SourceScope::Temporary {
            // Include the ref in the hash so each pinned ref gets its own checkout.
            return self.get_temporary_dir(
                &format!("git-{}", source.host),
                Some(&source.path),
                source.r#ref.as_deref(),
            );
        }
        let install_root = self
            .get_git_install_root(scope)?
            .ok_or_else(|| anyhow!("Missing git install root"))?;
        self.resolve_managed_path(&install_root, &[&source.host, &source.path])
    }

    fn get_git_install_root(&self, scope: SourceScope) -> anyhow::Result<Option<String>> {
        match scope {
            SourceScope::Temporary => Ok(None),
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(Some(node_join(&[&self.cwd, CONFIG_DIR_NAME, "git"])))
            }
            SourceScope::User => Ok(Some(node_join(&[&self.agent_dir, "git"]))),
        }
    }

    fn get_temporary_dir(&self, prefix: &str, suffix: Option<&str>, r#ref: Option<&str>) -> anyhow::Result<String> {
        use sha2::Digest;
        let root = self.resolve_managed_path(&get_extension_temp_folder(&self.agent_dir), &[prefix])?;
        let ref_part = match r#ref {
            Some(r) if !r.is_empty() => format!("@{r}"),
            _ => String::new(),
        };
        let input = format!("{prefix}-{}{ref_part}", suffix.unwrap_or(""));
        let digest = sha2::Sha256::digest(input.as_bytes());
        let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..8].to_string();
        self.resolve_managed_path(&root, &[&hash, suffix.unwrap_or("")])
    }

    fn resolve_managed_path(&self, root: &str, parts: &[&str]) -> anyhow::Result<String> {
        let resolved_root = node_resolve(&[root]);
        let mut segments: Vec<&str> = vec![&resolved_root];
        segments.extend_from_slice(parts);
        let resolved_path = node_resolve(&segments);
        if resolved_path != resolved_root
            && !resolved_path.starts_with(&format!("{resolved_root}{MAIN_SEPARATOR}"))
        {
            bail!("Refusing to use path outside package install root: {resolved_path}");
        }
        Ok(resolved_path)
    }

    fn get_base_dir_for_scope(&self, scope: SourceScope) -> anyhow::Result<String> {
        match scope {
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(node_join(&[&self.cwd, CONFIG_DIR_NAME]))
            }
            SourceScope::User => Ok(self.agent_dir.clone()),
            SourceScope::Temporary => Ok(self.cwd.clone()),
        }
    }

    fn path_options() -> PathInputOptions {
        PathInputOptions {
            home_dir: Some(get_home_dir()),
            trim: Some(true),
            ..Default::default()
        }
    }

    fn resolve_path(&self, input: &str) -> String {
        resolve_path(input, Some(&self.cwd), Some(&Self::path_options()))
    }

    fn resolve_path_from_base(&self, input: &str, base_dir: &str) -> String {
        resolve_path(input, Some(base_dir), Some(&Self::path_options()))
    }

    fn collect_package_resources(
        &self,
        package_root: &str,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for resource_type in RESOURCE_TYPES {
                let patterns = filter.patterns(resource_type);
                let target = Self::get_target_map(accumulator, resource_type);
                if filter.autoload == Some(false) {
                    self.apply_package_delta_filter(
                        package_root,
                        patterns.map(Vec::as_slice).unwrap_or(&[]),
                        resource_type,
                        target,
                        metadata,
                    );
                } else if let Some(patterns) = patterns {
                    self.apply_package_filter(package_root, patterns, resource_type, target, metadata);
                } else {
                    self.collect_default_resources(package_root, resource_type, target, metadata);
                }
            }
            return true;
        }

        if let Some(manifest) = read_pi_manifest(&node_join(&[package_root, "package.json"])) {
            for resource_type in RESOURCE_TYPES {
                let entries = resource_type.manifest_entries(&manifest).cloned();
                let target = Self::get_target_map(accumulator, resource_type);
                self.add_manifest_entries(entries.as_deref(), package_root, resource_type, target, metadata);
            }
            return true;
        }

        let mut has_any_dir = false;
        for resource_type in RESOURCE_TYPES {
            let dir = node_join(&[package_root, resource_type.as_str()]);
            if exists(&dir) {
                // Collect all files from the directory (all enabled by default)
                let files = collect_resource_files(&dir, resource_type);
                let target = Self::get_target_map(accumulator, resource_type);
                for f in files {
                    Self::add_resource(target, &f, metadata, true);
                }
                has_any_dir = true;
            }
        }
        has_any_dir
    }

    fn collect_default_resources(
        &self,
        package_root: &str,
        resource_type: ResourceType,
        target: &mut IndexMap<String, ResourceEntry>,
        metadata: &PathMetadata,
    ) {
        let manifest = read_pi_manifest(&node_join(&[package_root, "package.json"]));
        if let Some(entries) = manifest.as_ref().and_then(|m| resource_type.manifest_entries(m)) {
            self.add_manifest_entries(Some(entries), package_root, resource_type, target, metadata);
            return;
        }
        let dir = node_join(&[package_root, resource_type.as_str()]);
        if exists(&dir) {
            // Collect all files from the directory (all enabled by default)
            for f in collect_resource_files(&dir, resource_type) {
                Self::add_resource(target, &f, metadata, true);
            }
        }
    }

    fn apply_package_filter(
        &self,
        package_root: &str,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut IndexMap<String, ResourceEntry>,
        metadata: &PathMetadata,
    ) {
        let all_files = self.collect_manifest_files(package_root, resource_type);

        if user_patterns.is_empty() {
            // Empty array explicitly disables all resources of this type
            for f in &all_files {
                Self::add_resource(target, f, metadata, false);
            }
            return;
        }

        // Apply user patterns
        let enabled_by_user: HashSet<String> = apply_patterns(&all_files, user_patterns, package_root)
            .into_iter()
            .collect();
        for f in &all_files {
            Self::add_resource(target, f, metadata, enabled_by_user.contains(f));
        }
    }

    fn apply_package_delta_filter(
        &self,
        package_root: &str,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut IndexMap<String, ResourceEntry>,
        metadata: &PathMetadata,
    ) {
        if user_patterns.is_empty() {
            return;
        }
        let all_files = self.collect_manifest_files(package_root, resource_type);
        let enabled_by_user = apply_autoload_disabled_patterns(&all_files, user_patterns, package_root);
        for (file_path, enabled) in enabled_by_user {
            Self::add_resource(target, &file_path, metadata, enabled);
        }
    }

    /// Collect all files from a package for a resource type, applying manifest patterns.
    /// Returns the files that pass the manifest's own patterns.
    fn collect_manifest_files(&self, package_root: &str, resource_type: ResourceType) -> Vec<String> {
        let manifest = read_pi_manifest(&node_join(&[package_root, "package.json"]));
        if let Some(entries) = manifest
            .as_ref()
            .and_then(|m| resource_type.manifest_entries(m))
            .filter(|e| !e.is_empty())
        {
            let all_files = self.collect_files_from_manifest_entries(entries, package_root, resource_type);
            let manifest_patterns: Vec<String> =
                entries.iter().filter(|e| is_override_pattern(e)).cloned().collect();
            return if manifest_patterns.is_empty() {
                let mut seen = HashSet::new();
                all_files.into_iter().filter(|f| seen.insert(f.clone())).collect()
            } else {
                apply_patterns(&all_files, &manifest_patterns, package_root)
            };
        }

        let convention_dir = node_join(&[package_root, resource_type.as_str()]);
        if !exists(&convention_dir) {
            return Vec::new();
        }
        collect_resource_files(&convention_dir, resource_type)
    }

    fn add_manifest_entries(
        &self,
        entries: Option<&[String]>,
        root: &str,
        resource_type: ResourceType,
        target: &mut IndexMap<String, ResourceEntry>,
        metadata: &PathMetadata,
    ) {
        let Some(entries) = entries else {
            return;
        };
        let all_files = self.collect_files_from_manifest_entries(entries, root, resource_type);
        let patterns: Vec<String> = entries.iter().filter(|e| is_override_pattern(e)).cloned().collect();
        let enabled_paths: HashSet<String> = apply_patterns(&all_files, &patterns, root).into_iter().collect();
        for f in &all_files {
            if enabled_paths.contains(f) {
                Self::add_resource(target, f, metadata, true);
            }
        }
    }

    fn collect_files_from_manifest_entries(
        &self,
        entries: &[String],
        root: &str,
        resource_type: ResourceType,
    ) -> Vec<String> {
        let resolved: Vec<String> = entries
            .iter()
            .filter(|entry| !is_override_pattern(entry))
            .flat_map(|entry| {
                if !glob::has_glob_pattern(entry) {
                    vec![node_resolve(&[root, entry])]
                } else {
                    expand_package_glob(entry, root)
                }
            })
            .collect();
        self.collect_files_from_paths(&resolved, resource_type)
    }

    fn resolve_local_entries(
        &self,
        entries: &[String],
        resource_type: ResourceType,
        target: &mut IndexMap<String, ResourceEntry>,
        metadata: &PathMetadata,
        base_dir: &str,
    ) {
        if entries.is_empty() {
            return;
        }

        // Collect all files from plain entries (non-pattern entries)
        let (plain, patterns) = split_patterns(entries);
        let resolved_plain: Vec<String> = plain
            .iter()
            .map(|p| self.resolve_path_from_base(p, base_dir))
            .collect();
        let all_files = self.collect_files_from_paths(&resolved_plain, resource_type);

        // Determine which files are enabled based on patterns
        let enabled_paths: HashSet<String> = apply_patterns(&all_files, &patterns, base_dir)
            .into_iter()
            .collect();

        // Add all files with their enabled state
        for f in &all_files {
            Self::add_resource(target, f, metadata, enabled_paths.contains(f));
        }
    }

    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        global_settings: &Settings,
        project_settings: &Settings,
        global_base_dir: &str,
        project_base_dir: &str,
    ) {
        let user_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(global_base_dir.to_string()),
            package_root: None,
        };
        let project_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.to_string()),
            package_root: None,
        };

        let user_overrides = |t: ResourceType| t.settings_entries(global_settings);
        let project_overrides = |t: ResourceType| t.settings_entries(project_settings);
        let user_dir = |t: ResourceType| node_join(&[global_base_dir, t.as_str()]);
        let project_dir = |t: ResourceType| node_join(&[project_base_dir, t.as_str()]);

        let user_agents_skills_dir = node_join(&[&get_home_dir(), ".agents", "skills"]);
        let project_trusted = self.settings_manager.is_project_trusted();
        let project_agents_skill_dirs: Vec<String> = if project_trusted {
            collect_ancestor_agents_skill_dirs(&self.cwd)
                .into_iter()
                .filter(|dir| node_resolve(&[dir]) != node_resolve(&[&user_agents_skills_dir]))
                .collect()
        } else {
            Vec::new()
        };

        let add_resources = |accumulator: &mut ResourceAccumulator,
                             resource_type: ResourceType,
                             paths: Vec<String>,
                             metadata: &PathMetadata,
                             overrides: &[String],
                             base_dir: &str| {
            let target = Self::get_target_map(accumulator, resource_type);
            for path in paths {
                let enabled = is_enabled_by_overrides(&path, overrides, base_dir);
                Self::add_resource(target, &path, metadata, enabled);
            }
        };

        if project_trusted {
            // Project extensions from .pi/
            add_resources(
                accumulator,
                ResourceType::Extensions,
                collect_auto_extension_entries(&project_dir(ResourceType::Extensions)),
                &project_metadata,
                &project_overrides(ResourceType::Extensions),
                project_base_dir,
            );
            // Project skills from .pi/
            add_resources(
                accumulator,
                ResourceType::Skills,
                collect_auto_skill_entries(&project_dir(ResourceType::Skills), SkillDiscoveryMode::Pi),
                &project_metadata,
                &project_overrides(ResourceType::Skills),
                project_base_dir,
            );
        }

        // Project skills from .agents/ (each with its own baseDir)
        for agents_skills_dir in &project_agents_skill_dirs {
            let agents_base_dir = node_dirname(agents_skills_dir); // the .agents directory
            let agents_metadata = PathMetadata {
                base_dir: Some(agents_base_dir.clone()),
                ..project_metadata.clone()
            };
            add_resources(
                accumulator,
                ResourceType::Skills,
                collect_auto_skill_entries(agents_skills_dir, SkillDiscoveryMode::Agents),
                &agents_metadata,
                &project_overrides(ResourceType::Skills),
                &agents_base_dir,
            );
        }

        if project_trusted {
            add_resources(
                accumulator,
                ResourceType::Prompts,
                collect_auto_prompt_entries(&project_dir(ResourceType::Prompts)),
                &project_metadata,
                &project_overrides(ResourceType::Prompts),
                project_base_dir,
            );
            add_resources(
                accumulator,
                ResourceType::Themes,
                collect_auto_theme_entries(&project_dir(ResourceType::Themes)),
                &project_metadata,
                &project_overrides(ResourceType::Themes),
                project_base_dir,
            );
        }

        // User extensions from ~/.pi/agent/
        add_resources(
            accumulator,
            ResourceType::Extensions,
            collect_auto_extension_entries(&user_dir(ResourceType::Extensions)),
            &user_metadata,
            &user_overrides(ResourceType::Extensions),
            global_base_dir,
        );

        // User skills from ~/.pi/agent/
        add_resources(
            accumulator,
            ResourceType::Skills,
            collect_auto_skill_entries(&user_dir(ResourceType::Skills), SkillDiscoveryMode::Pi),
            &user_metadata,
            &user_overrides(ResourceType::Skills),
            global_base_dir,
        );

        // User skills from ~/.agents/ (with its own baseDir)
        let user_agents_base_dir = node_dirname(&user_agents_skills_dir);
        let user_agents_metadata = PathMetadata {
            base_dir: Some(user_agents_base_dir.clone()),
            ..user_metadata.clone()
        };
        add_resources(
            accumulator,
            ResourceType::Skills,
            collect_auto_skill_entries(&user_agents_skills_dir, SkillDiscoveryMode::Agents),
            &user_agents_metadata,
            &user_overrides(ResourceType::Skills),
            &user_agents_base_dir,
        );

        add_resources(
            accumulator,
            ResourceType::Prompts,
            collect_auto_prompt_entries(&user_dir(ResourceType::Prompts)),
            &user_metadata,
            &user_overrides(ResourceType::Prompts),
            global_base_dir,
        );
        add_resources(
            accumulator,
            ResourceType::Themes,
            collect_auto_theme_entries(&user_dir(ResourceType::Themes)),
            &user_metadata,
            &user_overrides(ResourceType::Themes),
            global_base_dir,
        );
    }

    fn collect_files_from_paths(&self, paths: &[String], resource_type: ResourceType) -> Vec<String> {
        let mut files = Vec::new();
        for p in paths {
            let Ok(stats) = std::fs::metadata(p) else {
                continue;
            };
            if stats.is_file() {
                files.push(p.clone());
            } else if stats.is_dir() {
                files.extend(collect_resource_files(p, resource_type));
            }
        }
        files
    }

    fn get_target_map(
        accumulator: &mut ResourceAccumulator,
        resource_type: ResourceType,
    ) -> &mut IndexMap<String, ResourceEntry> {
        match resource_type {
            ResourceType::Extensions => &mut accumulator.extensions,
            ResourceType::Skills => &mut accumulator.skills,
            ResourceType::Prompts => &mut accumulator.prompts,
            ResourceType::Themes => &mut accumulator.themes,
        }
    }

    fn add_resource(
        map: &mut IndexMap<String, ResourceEntry>,
        path: &str,
        metadata: &PathMetadata,
        enabled: bool,
    ) {
        if path.is_empty() {
            return;
        }
        if !map.contains_key(path) {
            map.insert(
                path.to_string(),
                ResourceEntry {
                    metadata: metadata.clone(),
                    enabled,
                },
            );
        }
    }

    fn to_resolved_paths(accumulator: ResourceAccumulator) -> ResolvedPaths {
        fn map_to_resolved(entries: IndexMap<String, ResourceEntry>) -> Vec<ResolvedResource> {
            let mut resolved: Vec<ResolvedResource> = entries
                .into_iter()
                .map(|(path, entry)| ResolvedResource {
                    path,
                    enabled: entry.enabled,
                    metadata: entry.metadata,
                })
                .collect();
            // Stable sort, like Array.prototype.sort.
            resolved.sort_by_key(|r| resource_precedence_rank(&r.metadata));
            let mut seen = HashSet::new();
            resolved.retain(|entry| seen.insert(canonicalize_path(&entry.path)));
            resolved
        }

        ResolvedPaths {
            extensions: map_to_resolved(accumulator.extensions),
            skills: map_to_resolved(accumulator.skills),
            prompts: map_to_resolved(accumulator.prompts),
            themes: map_to_resolved(accumulator.themes),
            install_failures: None,
        }
    }
}

/// The checkout target computed by `getLocalGitUpdateTarget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitUpdateTargetRef {
    pub r#ref: String,
    pub head: String,
    pub fetch_args: Vec<String>,
}

/// Match `^([0-9a-f]{40})\s+<name>?` on one `git ls-remote` line.
fn parse_ls_remote_line(line: &str, name: Option<&str>) -> Option<String> {
    let hash = line.get(..40)?;
    if !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let rest = &line[40..];
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    if let Some(name) = name
        && rest.trim() != name
    {
        return None;
    }
    Some(hash.to_string())
}

/// `runWithConcurrency`: run at most `limit` futures at a time, results in input order.
async fn run_with_concurrency<I, F, T>(tasks: I, limit: usize) -> Vec<T>
where
    I: IntoIterator<Item = F>,
    F: std::future::Future<Output = T>,
{
    let tasks: Vec<F> = tasks.into_iter().collect();
    if tasks.is_empty() {
        return Vec::new();
    }
    let worker_count = limit.clamp(1, tasks.len());
    futures::stream::iter(tasks)
        .buffered(worker_count)
        .collect()
        .await
}

#[async_trait::async_trait]
impl PackageManager for DefaultPackageManager {
    async fn resolve(&self, on_missing: Option<MissingSourceHandler>) -> anyhow::Result<ResolvedPaths> {
        self.resolve_impl(on_missing).await
    }
    async fn install(
        &self,
        source: &str,
        options: Option<&PackageManagerInstallOptions>,
    ) -> anyhow::Result<()> {
        self.install_impl(source, options.and_then(|o| o.local)).await
    }
    async fn install_and_persist(
        &self,
        source: &str,
        options: Option<&PackageManagerInstallAndPersistOptions>,
    ) -> anyhow::Result<()> {
        let local = options.and_then(|o| o.local);
        self.install_impl(source, local).await?;
        self.add_source_to_settings_impl(source, local)?;
        Ok(())
    }
    async fn remove(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveOptions>,
    ) -> anyhow::Result<()> {
        self.remove_impl(source, options.and_then(|o| o.local)).await
    }
    async fn remove_and_persist(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveAndPersistOptions>,
    ) -> anyhow::Result<bool> {
        let local = options.and_then(|o| o.local);
        self.remove_impl(source, local).await?;
        self.remove_source_from_settings_impl(source, local)
    }
    async fn update(&self, source: Option<&str>) -> anyhow::Result<()> {
        self.update_impl(source).await
    }
    fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        self.list_configured_packages_impl()
    }
    async fn resolve_extension_sources(
        &self,
        sources: &[String],
        options: Option<&PackageManagerResolveExtensionSourcesOptions>,
    ) -> anyhow::Result<ResolvedPaths> {
        self.resolve_extension_sources_impl(
            sources,
            options.and_then(|o| o.local),
            options.and_then(|o| o.temporary),
        )
        .await
    }
    fn add_source_to_settings(
        &self,
        source: &str,
        options: Option<&PackageManagerAddSourceToSettingsOptions>,
    ) -> anyhow::Result<bool> {
        self.add_source_to_settings_impl(source, options.and_then(|o| o.local))
    }
    fn remove_source_from_settings(
        &self,
        source: &str,
        options: Option<&PackageManagerRemoveSourceFromSettingsOptions>,
    ) -> anyhow::Result<bool> {
        self.remove_source_from_settings_impl(source, options.and_then(|o| o.local))
    }
    fn set_progress_callback(&self, callback: Option<&ProgressCallback>) {
        *self.progress_callback.write() = callback.cloned();
    }
    fn get_installed_path(&self, source: &str, scope: SettingsScope) -> anyhow::Result<Option<String>> {
        self.get_installed_path_impl(source, installed_to_source_scope(scope))
    }
}
