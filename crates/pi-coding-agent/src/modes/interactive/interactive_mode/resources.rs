//! Loaded-resources listing and path formatting (TS `showLoadedResources` and its helpers).

use super::*;
use crate::core::diagnostics::{ResourceDiagnostic, ResourceDiagnosticType};
use crate::core::source_info::{SourceInfo, SourceScope};
use crate::utils::paths::{node_relative, node_resolve};
use std::collections::HashMap;

/// An item with a path and its source.
#[derive(Clone)]
pub(crate) struct PathItem {
    pub(crate) path: String,
    pub(crate) source_info: Option<SourceInfo>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeGroupKind {
    User,
    Project,
    Path,
}

impl ScopeGroupKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Path => "path",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScopeGroup {
    scope: ScopeGroupKind,
    paths: Vec<PathItem>,
    packages: IndexMap<String, Vec<PathItem>>,
}

fn home_dir() -> String {
    dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// JS `localeCompare` (ICU root collation) for sorting labels.
pub(crate) fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    pi_tui::collation::locale_compare(a, b)
}

/// `path.posix.parse(p)`: `(dir, name)`.
fn posix_parse(p: &str) -> (String, String) {
    let (dir, base) = match p.rfind('/') {
        Some(index) => (p[..index].to_string(), &p[index + 1..]),
        None => (String::new(), p),
    };
    let name = match base.rfind('.') {
        Some(index) if index > 0 => base[..index].to_string(),
        _ => base.to_string(),
    };
    (dir, name)
}

fn source_of(info: Option<&SourceInfo>) -> String {
    info.map(|s| s.source.clone()).unwrap_or_default()
}

pub(crate) fn is_package_source(info: Option<&SourceInfo>) -> bool {
    let source = source_of(info);
    source.starts_with("npm:") || source.starts_with("git:")
}

impl Inner {
    pub(crate) fn format_display_path(&self, p: &str) -> String {
        let home = home_dir();
        if !home.is_empty()
            && let Some(rest) = p.strip_prefix(&home)
        {
            return format!("~{rest}");
        }
        p.to_string()
    }

    fn format_extension_display_path(&self, p: &str) -> String {
        let result = self.format_display_path(p);
        let result = result.strip_suffix("/index.ts").map(str::to_string).unwrap_or(result);
        result.strip_suffix("/index.js").map(str::to_string).unwrap_or(result)
    }

    fn format_context_path(&self, p: &str) -> String {
        let cwd = node_resolve(&[&self.session_manager().get_cwd()]);
        let absolute = if std::path::Path::new(p).is_absolute() { node_resolve(&[p]) } else { node_resolve(&[&cwd, p]) };
        if let Some(relative) = crate::utils::paths::get_cwd_relative_path(&absolute, &cwd) {
            return relative;
        }
        self.format_display_path(&absolute)
    }

    /// Get a short path relative to the package root for display.
    fn get_short_path(&self, full_path: &str, source_info: Option<&SourceInfo>) -> String {
        static NPM_ROOT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"^(.*/node_modules)/(@?[^/]+(?:/[^/]+)?)$").unwrap());
        static NPM: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"node_modules/(@?[^/]+(?:/[^/]+)?)/(.*)").unwrap());
        static GIT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"git/[^/]+/[^/]+/(.*)").unwrap());
        let normalized_full_path = full_path.replace('\\', "/");
        if let Some(info) = source_info
            && let Some(base_dir) = &info.base_dir
            && is_package_source(Some(info))
        {
            let normalized_base_dir = base_dir.replace('\\', "/");
            if let Some(captures) = NPM_ROOT.captures(&normalized_base_dir)
                && let Some(root) = captures.get(1)
                && normalized_full_path.starts_with(&format!("{}/", root.as_str()))
            {
                return node_relative(&normalized_base_dir, &normalized_full_path);
            }
            let relative = node_relative(&node_resolve(&[base_dir]), &node_resolve(&[full_path]));
            if !relative.is_empty()
                && relative != "."
                && !relative.starts_with("..")
                && !std::path::Path::new(&relative).is_absolute()
            {
                return relative.replace('\\', "/");
            }
        }
        let source = source_of(source_info);
        if source.starts_with("npm:")
            && let Some(captures) = NPM.captures(&normalized_full_path)
        {
            return captures[2].to_string();
        }
        if source.starts_with("git:")
            && let Some(captures) = GIT.captures(&normalized_full_path)
        {
            return captures[1].to_string();
        }
        self.format_display_path(full_path)
    }

    fn get_compact_path_label(&self, resource_path: &str, source_info: Option<&SourceInfo>) -> String {
        let short_path = self.get_short_path(resource_path, source_info);
        let normalized = short_path.replace('\\', "/");
        normalized
            .split('/').rfind(|s| !s.is_empty() && *s != "~")
            .map(str::to_string)
            .unwrap_or(short_path)
    }

    fn get_compact_package_source_label(&self, source_info: Option<&SourceInfo>) -> String {
        let source = source_of(source_info);
        if let Some(rest) = source.strip_prefix("npm:") {
            return if rest.is_empty() { source.clone() } else { rest.to_string() };
        }
        if let Some(git) = crate::utils::git::parse_git_url(&source) {
            return if git.path.is_empty() { source } else { git.path };
        }
        source
    }

    fn get_compact_extension_label(&self, resource_path: &str, source_info: Option<&SourceInfo>) -> String {
        if !is_package_source(source_info) {
            return self.get_compact_path_label(resource_path, source_info);
        }
        let source_label = self.get_compact_package_source_label(source_info);
        if source_label.is_empty() {
            return self.get_compact_path_label(resource_path, source_info);
        }
        let short_path = self.get_short_path(resource_path, source_info).replace('\\', "/");
        let package_path = short_path.strip_prefix("extensions/").unwrap_or(&short_path).to_string();
        let (dir, name) = posix_parse(&package_path);
        if name == "index" {
            return if dir.is_empty() || dir == "." { source_label } else { format!("{source_label}:{dir}") };
        }
        format!("{source_label}:{package_path}")
    }

    fn get_compact_display_path_segments(&self, resource_path: &str) -> Vec<String> {
        self.format_display_path(resource_path)
            .replace('\\', "/")
            .split('/')
            .filter(|s| !s.is_empty() && *s != "~")
            .map(str::to_string)
            .collect()
    }

    fn get_compact_non_package_extension_label(&self, resource_path: &str, index: usize, all: &[(String, Vec<String>)]) -> String {
        let Some((_, segments)) = all.get(index).filter(|(_, s)| !s.is_empty()) else {
            return self.get_compact_path_label(resource_path, None);
        };
        let tail = |segments: &[String], count: usize| -> String {
            let start = segments.len().saturating_sub(count);
            segments[start..].join("/")
        };
        for count in 1..=segments.len() {
            let candidate = tail(segments, count);
            let unique = all.iter().enumerate().all(|(i, (_, other))| i == index || tail(other, count) != candidate);
            if unique {
                return candidate;
            }
        }
        segments.join("/")
    }

    fn get_compact_extension_labels(&self, extensions: &[PathItem]) -> Vec<String> {
        let non_package: Vec<(String, Vec<String>)> = extensions
            .iter()
            .filter(|e| !is_package_source(e.source_info.as_ref()))
            .map(|e| {
                let mut segments = self.get_compact_display_path_segments(&e.path);
                if segments.len() > 1 && matches!(segments.last().map(String::as_str), Some("index.ts" | "index.js")) {
                    segments.pop();
                }
                (e.path.clone(), segments)
            })
            .collect();
        extensions
            .iter()
            .map(|e| {
                if is_package_source(e.source_info.as_ref()) {
                    return self.get_compact_extension_label(&e.path, e.source_info.as_ref());
                }
                match non_package.iter().position(|(p, _)| *p == e.path) {
                    Some(index) => self.get_compact_non_package_extension_label(&e.path, index, &non_package),
                    None => self.get_compact_path_label(&e.path, e.source_info.as_ref()),
                }
            })
            .collect()
    }

    fn get_display_source_info(&self, source_info: Option<&SourceInfo>) -> (String, Option<String>) {
        let source = source_info.map(|s| s.source.clone()).unwrap_or_else(|| "local".to_string());
        let scope = source_info.map(|s| s.scope).unwrap_or(SourceScope::Project);
        if source == "local" {
            return match scope {
                SourceScope::User => ("user".to_string(), None),
                SourceScope::Project => ("project".to_string(), None),
                SourceScope::Temporary => ("path".to_string(), Some("temp".to_string())),
            };
        }
        if source == "cli" {
            return ("path".to_string(), (scope == SourceScope::Temporary).then(|| "temp".to_string()));
        }
        let scope_label = match scope {
            SourceScope::User => "user",
            SourceScope::Project => "project",
            SourceScope::Temporary => "temp",
        };
        (source, Some(scope_label.to_string()))
    }

    fn get_scope_group(&self, source_info: Option<&SourceInfo>) -> ScopeGroupKind {
        let source = source_info.map(|s| s.source.clone()).unwrap_or_else(|| "local".to_string());
        let scope = source_info.map(|s| s.scope).unwrap_or(SourceScope::Project);
        if source == "cli" || scope == SourceScope::Temporary {
            return ScopeGroupKind::Path;
        }
        match scope {
            SourceScope::User => ScopeGroupKind::User,
            SourceScope::Project => ScopeGroupKind::Project,
            SourceScope::Temporary => ScopeGroupKind::Path,
        }
    }

    fn build_scope_groups(&self, items: &[PathItem]) -> Vec<ScopeGroup> {
        let mut groups = [ScopeGroupKind::Project, ScopeGroupKind::User, ScopeGroupKind::Path]
            .map(|scope| ScopeGroup { scope, paths: Vec::new(), packages: IndexMap::new() });
        for item in items {
            let kind = self.get_scope_group(item.source_info.as_ref());
            let group = groups.iter_mut().find(|g| g.scope == kind).expect("all scopes present");
            let source = item.source_info.as_ref().map(|s| s.source.clone()).unwrap_or_else(|| "local".to_string());
            if is_package_source(item.source_info.as_ref()) {
                group.packages.entry(source).or_default().push(item.clone());
            } else {
                group.paths.push(item.clone());
            }
        }
        groups.into_iter().filter(|g| !g.paths.is_empty() || !g.packages.is_empty()).collect()
    }

    fn format_scope_groups(
        &self,
        groups: &[ScopeGroup],
        format_path: &dyn Fn(&PathItem) -> String,
        format_package_path: &dyn Fn(&PathItem, &str) -> String,
    ) -> String {
        let t = theme();
        let mut lines = Vec::new();
        for group in groups {
            lines.push(format!("  {}", t.fg(&ThemeColor::Accent, group.scope.as_str())));
            let mut paths = group.paths.clone();
            paths.sort_by(|a, b| locale_compare(&a.path, &b.path));
            for item in &paths {
                lines.push(t.fg(&ThemeColor::Dim, &format!("    {}", format_path(item))));
            }
            let mut packages: Vec<(&String, &Vec<PathItem>)> = group.packages.iter().collect();
            packages.sort_by(|a, b| locale_compare(a.0, b.0));
            for (source, items) in packages {
                lines.push(format!("    {}", t.fg(&ThemeColor::MdLink, source)));
                let mut items = items.clone();
                items.sort_by(|a, b| locale_compare(&a.path, &b.path));
                for item in &items {
                    lines.push(t.fg(&ThemeColor::Dim, &format!("      {}", format_package_path(item, source))));
                }
            }
        }
        lines.join("\n")
    }

    fn find_source_info_for_path(&self, p: &str, source_infos: &HashMap<String, SourceInfo>) -> Option<SourceInfo> {
        if let Some(exact) = source_infos.get(p) {
            return Some(exact.clone());
        }
        let mut current = p.to_string();
        while let Some(index) = current.rfind('/') {
            current.truncate(index);
            if let Some(parent) = source_infos.get(&current) {
                return Some(parent.clone());
            }
        }
        None
    }

    fn format_path_with_source(&self, p: &str, source_info: Option<&SourceInfo>) -> String {
        if let Some(info) = source_info {
            let short_path = self.get_short_path(p, Some(info));
            let (label, scope_label) = self.get_display_source_info(Some(info));
            let label_text = match scope_label {
                Some(scope) => format!("{label} ({scope})"),
                None => label,
            };
            return format!("{label_text} {short_path}");
        }
        self.format_display_path(p)
    }

    fn format_diagnostics(&self, diagnostics: &[ResourceDiagnostic], source_infos: &HashMap<String, SourceInfo>) -> String {
        let t = theme();
        let mut lines = Vec::new();
        let mut collisions: IndexMap<String, Vec<&ResourceDiagnostic>> = IndexMap::new();
        let mut others = Vec::new();
        for d in diagnostics {
            match (&d.r#type, &d.collision) {
                (ResourceDiagnosticType::Collision, Some(collision)) => {
                    collisions.entry(collision.name.clone()).or_default().push(d)
                }
                _ => others.push(d),
            }
        }
        for (name, list) in &collisions {
            let Some(first) = list.first().and_then(|d| d.collision.as_ref()) else { continue };
            lines.push(t.fg(&ThemeColor::Warning, &format!("  \"{name}\" collision:")));
            lines.push(t.fg(
                &ThemeColor::Dim,
                &format!(
                    "    {} {}",
                    t.fg(&ThemeColor::Success, "✓"),
                    self.format_path_with_source(
                        &first.winner_path,
                        self.find_source_info_for_path(&first.winner_path, source_infos).as_ref()
                    )
                ),
            ));
            for d in list {
                if let Some(collision) = &d.collision {
                    lines.push(t.fg(
                        &ThemeColor::Dim,
                        &format!(
                            "    {} {} (skipped)",
                            t.fg(&ThemeColor::Warning, "✗"),
                            self.format_path_with_source(
                                &collision.loser_path,
                                self.find_source_info_for_path(&collision.loser_path, source_infos).as_ref()
                            )
                        ),
                    ));
                }
            }
        }
        for d in others {
            let color = if d.r#type == ResourceDiagnosticType::Error { ThemeColor::Error } else { ThemeColor::Warning };
            if let Some(path) = &d.path {
                let formatted = self.format_path_with_source(path, self.find_source_info_for_path(path, source_infos).as_ref());
                lines.push(t.fg(&color, &format!("  {formatted}")));
                lines.push(t.fg(&color, &format!("    {}", d.message)));
            } else {
                lines.push(t.fg(&color, &format!("  {}", d.message)));
            }
        }
        lines.join("\n")
    }

    pub(crate) fn get_built_in_command_conflict_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        let builtin: HashSet<&str> =
            crate::core::slash_commands::BUILTIN_SLASH_COMMANDS.iter().map(|c| c.name.as_str()).collect();
        self.session()
            .extension_runner()
            .get_registered_commands()
            .into_iter()
            .filter(|c| builtin.contains(c.name.as_str()))
            .map(|c| ResourceDiagnostic {
                r#type: ResourceDiagnosticType::Warning,
                message: if c.invocation_name == c.name {
                    format!("Extension command '/{}' conflicts with built-in interactive command. Skipping in autocomplete.", c.name)
                } else {
                    format!(
                        "Extension command '/{}' conflicts with built-in interactive command. Available as '/{}'.",
                        c.name, c.invocation_name
                    )
                },
                path: Some(c.source_info.path.clone()),
                collision: None,
            })
            .collect()
    }

    pub(crate) fn show_loaded_resources(&self, extensions: Option<Vec<PathItem>>, force: bool, show_diagnostics_when_quiet: bool) {
        // Resource rendering is idempotent; chat clears no longer clear this separate container.
        self.loaded_resources_container.clear();
        let show_listing = force || self.should_show_startup_details();
        let show_diagnostics = show_listing || show_diagnostics_when_quiet;
        if !show_listing && !show_diagnostics {
            return;
        }

        let this = self.this();
        let section_header = |name: &str, color: ThemeColor| theme().fg(&color, &format!("[{name}]"));
        let format_compact_list = |items: Vec<String>, sort: bool| -> String {
            let mut labels: Vec<String> = items.into_iter().map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect();
            if sort {
                labels.sort_by(|a, b| locale_compare(a, b));
            }
            theme().fg(&ThemeColor::Dim, &format!("  {}", labels.join(", ")))
        };
        let expanded = self.get_startup_expansion_state();
        let add_loaded_section = |name: &str,
                                  collapsed: Arc<dyn Fn() -> String + Send + Sync>,
                                  expanded_body: Arc<dyn Fn() -> String + Send + Sync>| {
            let header_name = name.to_string();
            let header_name2 = name.to_string();
            let section = ExpandableText::new(
                Arc::new(move || format!("{}\n{}", section_header(&header_name, ThemeColor::MdHeading), collapsed())),
                Arc::new(move || format!("{}\n{}", section_header(&header_name2, ThemeColor::MdHeading), expanded_body())),
                Some(expanded),
                Some(0),
                Some(0),
            );
            self.loaded_resources_container.add_child(Arc::new(section));
            self.loaded_resources_container.add_child(Arc::new(Spacer::new(1)));
        };

        let session = self.session();
        let loader = session.resource_loader();
        let skills_result = loader.get_skills();
        let prompts_result = loader.get_prompts();
        let themes_result = loader.get_themes();
        let extensions = extensions.unwrap_or_else(|| {
            loader
                .get_extensions()
                .extensions
                .iter()
                .filter_map(|e| {
                    let e = e.read();
                    (!e.hidden.unwrap_or(false))
                        .then(|| PathItem { path: e.path.clone(), source_info: Some(e.source_info.clone()) })
                })
                .collect()
        });
        let mut source_infos: HashMap<String, SourceInfo> = HashMap::new();
        for e in &extensions {
            if let Some(info) = &e.source_info {
                source_infos.insert(e.path.clone(), info.clone());
            }
        }
        for skill in &skills_result.skills {
            source_infos.insert(skill.file_path.clone(), skill.source_info.clone());
        }
        for prompt in &prompts_result.prompts {
            source_infos.insert(prompt.file_path.clone(), prompt.source_info.clone());
        }
        for loaded in &themes_result.themes {
            if let (Some(path), Some(info)) = (loaded.source_path(), loaded.source_info()) {
                source_infos.insert(path, info);
            }
        }
        let source_infos = Arc::new(source_infos);

        if show_listing {
            let mut context_paths: Vec<String> = Vec::new();
            if let Some(source) = loader.get_system_prompt_source() {
                context_paths.push(source.path);
            }
            context_paths.extend(loader.get_append_system_prompt_sources().into_iter().map(|s| s.path));
            context_paths.extend(loader.get_agents_files().agents_files.into_iter().map(|f| f.path));
            if !context_paths.is_empty() {
                self.loaded_resources_container.add_child(Arc::new(Spacer::new(1)));
                let (t1, p1) = (this.clone(), context_paths.clone());
                let list: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
                    p1.iter().map(|p| theme().fg(&ThemeColor::Dim, &format!("  {}", t1.format_display_path(p)))).collect::<Vec<_>>().join("\n")
                });
                let (t2, p2) = (this.clone(), context_paths);
                let compact: Arc<dyn Fn() -> String + Send + Sync> =
                    Arc::new(move || format_compact_list(p2.iter().map(|p| t2.format_context_path(p)).collect(), false));
                add_loaded_section("Context", compact, list);
            }

            let skills = skills_result.skills.clone();
            if !skills.is_empty() {
                let items: Vec<PathItem> = skills
                    .iter()
                    .map(|s| PathItem { path: s.file_path.clone(), source_info: Some(s.source_info.clone()) })
                    .collect();
                let groups = self.build_scope_groups(&items);
                let t1 = this.clone();
                let list: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
                    let t = t1.clone();
                    let t_inner = t1.clone();
                    t.format_scope_groups(
                        &groups,
                        &move |item| t_inner.format_display_path(&item.path),
                        &|item, _| t1.get_short_path(&item.path, item.source_info.as_ref()),
                    )
                });
                let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
                let compact: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || format_compact_list(names.clone(), true));
                add_loaded_section("Skills", compact, list);
            }

            let templates = session.prompt_templates();
            if !templates.is_empty() {
                let items: Vec<PathItem> = templates
                    .iter()
                    .map(|t| PathItem { path: t.file_path.clone(), source_info: Some(t.source_info.clone()) })
                    .collect();
                let groups = self.build_scope_groups(&items);
                let by_path: Arc<HashMap<String, String>> =
                    Arc::new(templates.iter().map(|t| (t.file_path.clone(), t.name.clone())).collect());
                let t1 = this.clone();
                let list: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
                    let fmt = |item: &PathItem| match by_path.get(&item.path) {
                        Some(name) => format!("/{name}"),
                        None => t1.format_display_path(&item.path),
                    };
                    t1.format_scope_groups(&groups, &fmt, &|item, _| fmt(item))
                });
                let names: Vec<String> = templates.iter().map(|t| format!("/{}", t.name)).collect();
                let compact: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || format_compact_list(names.clone(), true));
                add_loaded_section("Prompts", compact, list);
            }

            if !extensions.is_empty() {
                let groups = self.build_scope_groups(&extensions);
                let t1 = this.clone();
                let list: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
                    t1.format_scope_groups(
                        &groups,
                        &|item| t1.format_extension_display_path(&item.path),
                        &|item, _| t1.format_extension_display_path(&t1.get_short_path(&item.path, item.source_info.as_ref())),
                    )
                });
                let labels = self.get_compact_extension_labels(&extensions);
                let compact: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || format_compact_list(labels.clone(), true));
                add_loaded_section("Extensions", compact, list);
            }
        }

        if show_diagnostics {
            let add_issues = |title: &'static str, diagnostics: Vec<ResourceDiagnostic>| {
                if diagnostics.is_empty() {
                    return;
                }
                let t1 = this.clone();
                let infos = source_infos.clone();
                self.loaded_resources_container.add_child(Arc::new(ThemedText::new(
                    Arc::new(move || {
                        format!("{}\n{}", theme().fg(&ThemeColor::Warning, title), t1.format_diagnostics(&diagnostics, &infos))
                    }),
                    Some(0),
                    Some(0),
                )));
                self.loaded_resources_container.add_child(Arc::new(Spacer::new(1)));
            };
            add_issues("[Skill conflicts]", skills_result.diagnostics.clone());
            add_issues("[Prompt conflicts]", prompts_result.diagnostics.clone());

            let mut extension_diagnostics = Vec::new();
            let extensions_result = loader.get_extensions();
            for error in &extensions_result.errors {
                extension_diagnostics.push(ResourceDiagnostic {
                    r#type: ResourceDiagnosticType::Error,
                    message: error.error.clone(),
                    path: Some(error.path.clone()),
                    collision: None,
                });
            }
            for warning in extensions_result.warnings.iter().flatten() {
                extension_diagnostics.push(ResourceDiagnostic {
                    r#type: ResourceDiagnosticType::Warning,
                    message: warning.warning.clone(),
                    path: Some(warning.path.clone()),
                    collision: None,
                });
            }
            let runner = session.extension_runner();
            extension_diagnostics.extend(runner.get_command_diagnostics());
            extension_diagnostics.extend(self.get_built_in_command_conflict_diagnostics());
            extension_diagnostics.extend(runner.get_shortcut_diagnostics());
            add_issues("[Extension issues]", extension_diagnostics);
            add_issues("[Theme conflicts]", themes_result.diagnostics.clone());
        }
    }
}
