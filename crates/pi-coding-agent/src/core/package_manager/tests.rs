//! Tests ported from `packages/coding-agent/test/package-manager.test.ts`,
//! `package-manager-ssh.test.ts`, `git-update.test.ts` and
//! `suite/regressions/7187-malformed-package-manifest.test.ts`.
//!
//! The TS tests spy on private `runCommand`/`runCommandCapture`/`runCommandSync`; here a
//! [`MockRunner`] implements [`PackageCommandRunner`] and records every call.

// Tests run on current-thread runtimes; the env lock is held across awaits on purpose.
#![allow(clippy::await_holding_lock)]

use super::*;
use crate::core::settings_manager::SettingsManager;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    super::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
}

type RunFn = Box<dyn Fn(&Call) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
type CaptureFn = Box<dyn Fn(&Call) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;
type SyncFn = Box<dyn Fn(&Call) -> anyhow::Result<String> + Send + Sync>;

struct MockRunner {
    runs: StdMutex<Vec<Call>>,
    captures: StdMutex<Vec<Call>>,
    syncs: StdMutex<Vec<Call>>,
    run_fn: StdMutex<Option<Arc<RunFn>>>,
    capture_fn: StdMutex<Option<Arc<CaptureFn>>>,
    sync_fn: StdMutex<Option<Arc<SyncFn>>>,
}

impl MockRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: StdMutex::new(Vec::new()),
            captures: StdMutex::new(Vec::new()),
            syncs: StdMutex::new(Vec::new()),
            run_fn: StdMutex::new(None),
            capture_fn: StdMutex::new(None),
            sync_fn: StdMutex::new(None),
        })
    }

    fn on_run(&self, f: impl Fn(&Call) -> anyhow::Result<()> + Send + Sync + 'static) {
        let f = Arc::new(f);
        let wrapped: RunFn = Box::new(move |call| {
            let result = f(call);
            Box::pin(async move { result })
        });
        *self.run_fn.lock().unwrap() = Some(Arc::new(wrapped));
    }

    fn on_run_async(&self, f: RunFn) {
        *self.run_fn.lock().unwrap() = Some(Arc::new(f));
    }

    fn on_capture(&self, f: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static) {
        let f = Arc::new(f);
        let wrapped: CaptureFn = Box::new(move |call| {
            let result = f(call);
            Box::pin(async move { result })
        });
        *self.capture_fn.lock().unwrap() = Some(Arc::new(wrapped));
    }

    fn on_sync(&self, f: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static) {
        *self.sync_fn.lock().unwrap() = Some(Arc::new(Box::new(f)));
    }

    fn runs(&self) -> Vec<Call> {
        self.runs.lock().unwrap().clone()
    }

    fn captures(&self) -> Vec<Call> {
        self.captures.lock().unwrap().clone()
    }

    fn syncs(&self) -> Vec<Call> {
        self.syncs.lock().unwrap().clone()
    }

    fn ran(&self, command: &str, args: &[&str], cwd: Option<&str>) -> bool {
        self.runs().contains(&call(command, args, cwd))
    }
}

fn call(command: &str, args: &[&str], cwd: Option<&str>) -> Call {
    Call {
        command: command.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: cwd.map(str::to_string),
    }
}

#[async_trait::async_trait]
impl PackageCommandRunner for MockRunner {
    async fn run(&self, command: &str, args: &[String], cwd: Option<&str>) -> anyhow::Result<()> {
        let c = Call {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(str::to_string),
        };
        self.runs.lock().unwrap().push(c.clone());
        let f = self.run_fn.lock().unwrap().clone();
        match f {
            Some(f) => f(&c).await,
            None => Ok(()),
        }
    }

    async fn run_capture(
        &self,
        command: &str,
        args: &[String],
        options: &CaptureOptions,
    ) -> anyhow::Result<String> {
        let c = Call {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: options.cwd.clone(),
        };
        self.captures.lock().unwrap().push(c.clone());
        let f = self.capture_fn.lock().unwrap().clone();
        match f {
            Some(f) => f(&c).await,
            None => bail!("Unexpected runCommandCapture args: {}", args.join(" ")),
        }
    }

    fn run_sync(&self, command: &str, args: &[String]) -> anyhow::Result<String> {
        let c = Call {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: None,
        };
        self.syncs.lock().unwrap().push(c.clone());
        let f = self.sync_fn.lock().unwrap().clone();
        match f {
            Some(f) => f(&c),
            None => bail!("legacy lookup unavailable"),
        }
    }

    fn command_exists(&self, _command: &str) -> bool {
        true
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    temp: String,
    agent: String,
    settings: Arc<SettingsManager>,
    runner: Arc<MockRunner>,
    pm: DefaultPackageManager,
}

impl Fixture {
    fn new() -> Self {
        Self::with_settings(json!({}))
    }

    fn with_settings(settings: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let temp = canonicalize_path(&dir.path().to_string_lossy());
        let agent = node_join(&[&temp, "agent"]);
        std::fs::create_dir_all(&agent).unwrap();
        let settings = SettingsManager::in_memory_json(settings.as_object().unwrap().clone(), None);
        let runner = MockRunner::new();
        let pm = DefaultPackageManager::with_command_runner(
            &PackageManagerOptions {
                cwd: temp.clone(),
                agent_dir: agent.clone(),
                settings_manager: settings.clone(),
                builtin_extensions: None,
            },
            runner.clone(),
        );
        Self {
            _dir: dir,
            temp,
            agent,
            settings,
            runner,
            pm,
        }
    }

    fn path(&self, parts: &[&str]) -> String {
        let mut all = vec![self.temp.as_str()];
        all.extend_from_slice(parts);
        node_join(&all)
    }

    fn agent_path(&self, parts: &[&str]) -> String {
        let mut all = vec![self.agent.as_str()];
        all.extend_from_slice(parts);
        node_join(&all)
    }
}

fn write(path: &str, content: &str) {
    std::fs::create_dir_all(node_dirname(path)).unwrap();
    std::fs::write(path, content).unwrap();
}

fn mkdir(path: &str) {
    std::fs::create_dir_all(path).unwrap();
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn pkg(value: serde_json::Value) -> PackageSource {
    serde_json::from_value(value).unwrap()
}

fn ends_with(path: &str, suffix: &str) -> bool {
    path.replace('\\', "/").ends_with(suffix)
}

fn is_enabled(r: &ResolvedResource, m: &str) -> bool {
    ends_with(&r.path, m) && r.enabled
}

fn is_disabled(r: &ResolvedResource, m: &str) -> bool {
    ends_with(&r.path, m) && !r.enabled
}

fn npm(pm: &DefaultPackageManager, source: &str) -> NpmSource {
    match pm.parse_source(source) {
        ParsedSource::NpmSource(n) => n,
        other => panic!("expected npm source, got {other:?}"),
    }
}

fn git(pm: &DefaultPackageManager, source: &str) -> GitSource {
    match pm.parse_source(source) {
        ParsedSource::GitSource(g) => g,
        other => panic!("expected git source, got {other:?}"),
    }
}

fn is_local(pm: &DefaultPackageManager, source: &str) -> bool {
    matches!(pm.parse_source(source), ParsedSource::LocalSource(_))
}

const EXT: &str = "export default function() {}";

// ---------------------------------------------------------------------------
// resolve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_returns_no_package_paths_without_sources() {
    let _g = env_lock();
    let f = Fixture::new();
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.is_empty());
    assert!(result.prompts.is_empty());
    assert!(result.themes.is_empty());
    assert!(result
        .skills
        .iter()
        .all(|r| r.metadata.source == "auto" && r.metadata.origin == SourceOrigin::TopLevel));
}

#[tokio::test]
async fn resolve_local_extension_paths_from_settings() {
    let _g = env_lock();
    let f = Fixture::new();
    let ext_path = f.agent_path(&["extensions", "my-extension.ts"]);
    write(&ext_path, EXT);
    f.settings.set_extension_paths(&s(&["extensions/my-extension.ts"]));
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| r.path == ext_path && r.enabled));
}

#[tokio::test]
async fn resolve_builtin_extensions_with_user_exclusions_and_project_overrides() {
    let _g = env_lock();
    let f = Fixture::new();
    let pm = DefaultPackageManager::with_command_runner(
        &PackageManagerOptions {
            cwd: f.temp.clone(),
            agent_dir: f.agent.clone(),
            settings_manager: f.settings.clone(),
            builtin_extensions: Some(s(&["mcp", "llama.cpp"])),
        },
        f.runner.clone(),
    );
    let builtins = || async {
        pm.resolve(None)
            .await
            .unwrap()
            .extensions
            .into_iter()
            .map(|r| (r.path, r.enabled, r.metadata.source, r.metadata.scope))
            .collect::<Vec<_>>()
    };
    let u = SourceScope::User;
    let p = SourceScope::Project;
    let b = || "builtin".to_string();
    assert_eq!(
        builtins().await,
        vec![("builtin:mcp".to_string(), true, b(), u), ("builtin:llama.cpp".to_string(), true, b(), u)]
    );
    f.settings.set_extension_paths(&s(&["-builtin:mcp"]));
    f.settings
        .set_project_extension_paths(&s(&["+builtin:mcp", "-builtin:llama.cpp"]))
        .unwrap();
    assert_eq!(
        builtins().await,
        vec![("builtin:mcp".to_string(), true, b(), p), ("builtin:llama.cpp".to_string(), false, b(), p)]
    );
    f.settings.set_project_extension_paths(&[]).unwrap();
    assert_eq!(
        builtins().await,
        vec![("builtin:mcp".to_string(), false, b(), u), ("builtin:llama.cpp".to_string(), true, b(), u)]
    );
}

#[tokio::test]
async fn resolve_skill_paths_from_settings_and_root_markdown_skills() {
    let _g = env_lock();
    let f = Fixture::new();
    let skill_file = f.agent_path(&["skills", "my-skill", "SKILL.md"]);
    write(&skill_file, "---\nname: test-skill\ndescription: A test skill\n---\nContent");
    let root_skill = f.agent_path(&["skills", "single-file.md"]);
    write(&root_skill, "---\nname: single-file\ndescription: Root\n---\nContent");
    f.settings.set_skill_paths(&s(&["skills"]));
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.skills.iter().any(|r| r.path == skill_file && r.enabled));
    assert!(result.skills.iter().any(|r| r.path == root_skill && r.enabled));
}

#[tokio::test]
async fn resolve_project_paths_relative_to_pi_dir() {
    let _g = env_lock();
    let f = Fixture::new();
    let ext_path = f.path(&[".pi", "extensions", "project-ext.ts"]);
    write(&ext_path, EXT);
    f.settings
        .set_project_extension_paths(&s(&["extensions/project-ext.ts"]))
        .unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| r.path == ext_path && r.enabled));
}

#[tokio::test]
async fn resolve_auto_discovered_prompts_with_overrides() {
    let _g = env_lock();
    let f = Fixture::new();
    let user_prompt = f.agent_path(&["prompts", "auto.md"]);
    write(&user_prompt, "Auto prompt");
    let project_prompt = f.path(&[".pi", "prompts", "is.md"]);
    write(&project_prompt, "Is prompt");
    f.settings.set_prompt_template_paths(&s(&["!prompts/auto.md"]));
    f.settings
        .set_project_prompt_template_paths(&s(&["!prompts/is.md"]))
        .unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.prompts.iter().any(|r| r.path == user_prompt && !r.enabled));
    assert!(result.prompts.iter().any(|r| r.path == project_prompt && !r.enabled));
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_symlinked_user_and_project_resources_once() {
    let _g = env_lock();
    let f = Fixture::new();
    let previous_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", &f.temp) };
    let shared = f.path(&["shared-resources"]);
    write(&node_join(&[&shared, "extensions", "shared.ts"]), EXT);
    write(
        &node_join(&[&shared, "skills", "shared-skill", "SKILL.md"]),
        "---\nname: shared-skill\ndescription: Shared skill\n---\nContent",
    );
    write(&node_join(&[&shared, "prompts", "shared.md"]), "Shared prompt");
    write(&node_join(&[&shared, "themes", "shared.json"]), "{\"name\":\"shared-theme\"}");
    mkdir(&f.path(&[".pi"]));
    for kind in ["extensions", "skills", "prompts", "themes"] {
        std::os::unix::fs::symlink(node_join(&[&shared, kind]), f.agent_path(&[kind])).unwrap();
        std::os::unix::fs::symlink(node_join(&[&shared, kind]), f.path(&[".pi", kind])).unwrap();
    }
    let result = f.pm.resolve(None).await;
    match previous_home {
        Some(home) => unsafe { std::env::set_var("HOME", home) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let result = result.unwrap();
    assert_eq!(
        (result.extensions.len(), result.skills.len(), result.prompts.len(), result.themes.len()),
        (1, 1, 1, 1)
    );
    assert_eq!(result.extensions[0].metadata.scope, SourceScope::Project);
    assert_eq!(result.skills[0].metadata.scope, SourceScope::Project);
    assert_eq!(result.prompts[0].metadata.scope, SourceScope::Project);
    assert_eq!(result.themes[0].metadata.scope, SourceScope::Project);
}

#[tokio::test]
async fn resolve_directory_with_pi_extensions_manifest_in_extensions_setting() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["my-extensions-pkg"]);
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "my-extensions-pkg", "pi": {"extensions": ["./extensions/clip.ts", "./extensions/cost.ts"]}})
            .to_string(),
    );
    for name in ["clip.ts", "cost.ts", "helper.ts"] {
        write(&node_join(&[&pkg_dir, "extensions", name]), EXT);
    }
    f.settings.set_extension_paths(std::slice::from_ref(&pkg_dir));
    let result = f.pm.resolve(None).await.unwrap();
    let clip = node_join(&[&pkg_dir, "extensions", "clip.ts"]);
    let cost = node_join(&[&pkg_dir, "extensions", "cost.ts"]);
    assert!(result.extensions.iter().any(|r| r.path == clip && r.enabled));
    assert!(result.extensions.iter().any(|r| r.path == cost && r.enabled));
    assert!(!result.extensions.iter().any(|r| ends_with(&r.path, "helper.ts")));
}

// ---------------------------------------------------------------------------
// auto-discovered skill metadata / .agents skills
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_discovered_skill_metadata_base_dirs() {
    let _g = env_lock();
    let f = Fixture::new();
    let previous_home = std::env::var("HOME").ok();
    let home = f.path(&["home"]);
    mkdir(&home);
    unsafe { std::env::set_var("HOME", &home) };
    // Make the project a git repo so ancestor scanning stops at it.
    mkdir(&f.path(&[".git"]));
    let user_skill = f.agent_path(&["skills", "user-skill", "SKILL.md"]);
    write(&user_skill, "---\nname: user-skill\ndescription: d\n---\n");
    let project_skill = f.path(&[".pi", "skills", "project-skill", "SKILL.md"]);
    write(&project_skill, "---\nname: project-skill\ndescription: d\n---\n");
    let home_agents_skill = node_join(&[&home, ".agents", "skills", "home-skill", "SKILL.md"]);
    write(&home_agents_skill, "---\nname: home-skill\ndescription: d\n---\n");
    let project_agents_skill = f.path(&[".agents", "skills", "agents-skill", "SKILL.md"]);
    write(&project_agents_skill, "---\nname: agents-skill\ndescription: d\n---\n");
    let root_md = f.path(&[".agents", "skills", "root.md"]);
    write(&root_md, "# not a skill");
    let nested_md = f.path(&[".agents", "skills", "group", "nested.md"]);
    write(&nested_md, "---\nname: nested\ndescription: d\n---\n");

    let result = f.pm.resolve(None).await;
    match previous_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let result = result.unwrap();
    let base = |path: &str| {
        result
            .skills
            .iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("missing {path}"))
            .metadata
            .clone()
    };
    assert_eq!(base(&user_skill).base_dir.as_deref(), Some(f.agent.as_str()));
    assert_eq!(base(&project_skill).base_dir, Some(f.path(&[".pi"])));
    assert_eq!(base(&home_agents_skill).base_dir, Some(node_join(&[&home, ".agents"])));
    assert_eq!(base(&home_agents_skill).scope, SourceScope::User);
    assert_eq!(base(&project_agents_skill).base_dir, Some(f.path(&[".agents"])));
    assert_eq!(base(&project_agents_skill).scope, SourceScope::Project);
    assert!(!result.skills.iter().any(|r| r.path == root_md));
    assert!(result.skills.iter().any(|r| r.path == nested_md));
}

#[tokio::test]
async fn scans_agents_skills_from_cwd_up_to_git_root() {
    let _g = env_lock();
    let f = Fixture::new();
    let repo = f.path(&["repo"]);
    mkdir(&node_join(&[&repo, ".git"]));
    let cwd = node_join(&[&repo, "packages", "feature"]);
    mkdir(&cwd);
    let repo_skill = node_join(&[&repo, ".agents", "skills", "repo-skill", "SKILL.md"]);
    write(&repo_skill, "---\nname: repo-skill\ndescription: d\n---\n");
    let cwd_skill = node_join(&[&cwd, ".agents", "skills", "cwd-skill", "SKILL.md"]);
    write(&cwd_skill, "---\nname: cwd-skill\ndescription: d\n---\n");
    let outside_skill = f.path(&[".agents", "skills", "outside", "SKILL.md"]);
    write(&outside_skill, "---\nname: outside\ndescription: d\n---\n");
    let pm = DefaultPackageManager::with_command_runner(
        &PackageManagerOptions {
            cwd: cwd.clone(),
            agent_dir: f.agent.clone(),
            settings_manager: f.settings.clone(),
            builtin_extensions: None,
        },
        f.runner.clone(),
    );
    let result = pm.resolve(None).await.unwrap();
    assert!(result.skills.iter().any(|r| r.path == repo_skill));
    assert!(result.skills.iter().any(|r| r.path == cwd_skill));
    assert!(!result.skills.iter().any(|r| r.path == outside_skill));
}

// ---------------------------------------------------------------------------
// ignore files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn respects_gitignore_in_skill_directories() {
    let _g = env_lock();
    let f = Fixture::new();
    let skills = f.agent_path(&["skills"]);
    write(&node_join(&[&skills, ".gitignore"]), "venv\n");
    write(
        &node_join(&[&skills, "good", "SKILL.md"]),
        "---\nname: good\ndescription: d\n---\n",
    );
    write(
        &node_join(&[&skills, "venv", "bad", "SKILL.md"]),
        "---\nname: bad\ndescription: d\n---\n",
    );
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.skills.iter().any(|r| ends_with(&r.path, "good/SKILL.md")));
    assert!(!result.skills.iter().any(|r| r.path.contains("venv")));
}

#[tokio::test]
async fn does_not_apply_parent_gitignore_to_pi_auto_discovery() {
    let _g = env_lock();
    let f = Fixture::new();
    write(&f.path(&[".gitignore"]), ".pi\n");
    let ext = f.path(&[".pi", "extensions", "ext.ts"]);
    write(&ext, EXT);
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| r.path == ext));
}

// ---------------------------------------------------------------------------
// resolveExtensionSources
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_extension_sources_local_paths_and_manifests() {
    let _g = env_lock();
    let f = Fixture::new();
    let ext_path = f.path(&["ext.ts"]);
    write(&ext_path, EXT);
    let result = f.pm.resolve_extension_sources(std::slice::from_ref(&ext_path), None).await.unwrap();
    assert!(result.extensions.iter().any(|r| r.path == ext_path && r.enabled));

    let pkg_dir = f.path(&["my-package"]);
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "my-package", "pi": {"extensions": ["./src/index.ts"], "skills": ["./skills"]}}).to_string(),
    );
    write(&node_join(&[&pkg_dir, "src", "index.ts"]), EXT);
    let skill = node_join(&[&pkg_dir, "skills", "my-skill", "SKILL.md"]);
    write(&skill, "---\nname: my-skill\ndescription: Test\n---\nContent");
    let result = f.pm.resolve_extension_sources(std::slice::from_ref(&pkg_dir), None).await.unwrap();
    let index = node_join(&[&pkg_dir, "src", "index.ts"]);
    assert!(result.extensions.iter().any(|r| r.path == index && r.enabled));
    assert!(result.skills.iter().any(|r| r.path == skill && r.enabled));
}

#[tokio::test]
async fn keeps_pi_manifest_entries_with_leading_tilde_package_relative() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["tilde-manifest-package"]);
    let direct_ext = node_join(&[&pkg_dir, "~extensions", "main.ts"]);
    let slash_ext = node_join(&[&pkg_dir, "~", "extensions", "alt.ts"]);
    let direct_skill = node_join(&[&pkg_dir, "~skills", "direct-skill", "SKILL.md"]);
    let slash_skill = node_join(&[&pkg_dir, "~", "skills", "slash-skill", "SKILL.md"]);
    write(&direct_ext, EXT);
    write(&slash_ext, EXT);
    write(&direct_skill, "---\nname: direct-skill\ndescription: Direct\n---\nContent");
    write(&slash_skill, "---\nname: slash-skill\ndescription: Slash\n---\nContent");
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "t", "pi": {"extensions": ["~extensions/main.ts", "~/extensions/alt.ts"], "skills": ["~skills", "~/skills"]}}).to_string(),
    );
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.extensions.iter().any(|r| r.path == direct_ext && r.enabled));
    assert!(result.extensions.iter().any(|r| r.path == slash_ext && r.enabled));
    assert!(result.skills.iter().any(|r| r.path == direct_skill && r.enabled));
    assert!(result.skills.iter().any(|r| r.path == slash_skill && r.enabled));
}

#[tokio::test]
async fn resolve_extension_sources_auto_discovery_layout_and_skill_roots() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["auto-pkg"]);
    write(&node_join(&[&pkg_dir, "extensions", "main.ts"]), EXT);
    write(&node_join(&[&pkg_dir, "themes", "dark.json"]), "{}");
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "main.ts")));
    assert!(result.themes.iter().any(|r| is_enabled(r, "dark.json")));

    let pkg_dir = f.path(&["skill-root-pkg"]);
    let root_skill = node_join(&[&pkg_dir, "skills", "root-skill", "SKILL.md"]);
    let nested_skill = node_join(&[&pkg_dir, "skills", "root-skill", "nested-skill", "SKILL.md"]);
    write(&root_skill, "---\nname: root-skill\ndescription: Root skill\n---\n");
    write(&nested_skill, "---\nname: nested-skill\ndescription: Nested skill\n---\n");
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.skills.iter().any(|r| r.path == root_skill && r.enabled));
    assert!(!result.skills.iter().any(|r| r.path == nested_skill));
}

#[tokio::test]
async fn progress_callback_not_called_for_local_paths() {
    let _g = env_lock();
    let f = Fixture::new();
    let events = Arc::new(StdMutex::new(Vec::<ProgressEvent>::new()));
    let sink = events.clone();
    let cb: ProgressCallback = Arc::new(move |e| sink.lock().unwrap().push(e));
    f.pm.set_progress_callback(Some(&cb));
    let ext = f.path(&["ext.ts"]);
    write(&ext, EXT);
    f.pm.resolve_extension_sources(&[ext], None).await.unwrap();
    assert!(events.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// command spawning
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn process_runner_preserves_argv_and_captures_output() {
    let value_with_space = "C:\\Users\\A B\\.pi\\npm";
    let output = ProcessCommandRunner
        .run_sync("sh", &s(&["-c", "printf '%s\\n' \"$1\"", "sh", value_with_space]))
        .unwrap();
    assert_eq!(output, value_with_space);
    let captured = ProcessCommandRunner
        .run_capture("sh", &s(&["-c", "echo abc123"]), &CaptureOptions::default())
        .await
        .unwrap();
    assert_eq!(captured, "abc123");
    let error = ProcessCommandRunner
        .run_capture(
            "sh",
            &s(&["-c", "sleep 5"]),
            &CaptureOptions {
                timeout_ms: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "sh -c sleep 5 timed out after 50ms");
    let error = ProcessCommandRunner
        .run_capture("sh", &s(&["-c", "echo oops >&2; exit 3"]), &CaptureOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "sh -c echo oops >&2; exit 3 failed with code 3: oops\n");
    let error = ProcessCommandRunner
        .run("sh", &s(&["-c", "exit 4"]), None)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "sh -c exit 4 failed with code 4");
}

// ---------------------------------------------------------------------------
// npmCommand
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uses_npm_command_argv_for_npm_installs() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["mise", "exec", "node@20", "--", "npm"]}));
    f.pm.install("npm:@scope/pkg", None).await.unwrap();
    let root = f.agent_path(&["npm"]);
    assert!(f.runner.ran(
        "mise",
        &["exec", "node@20", "--", "npm", "install", "@scope/pkg", "--prefix", &root, "--legacy-peer-deps"],
        None
    ));
    // ensureNpmProject wrote the managed project files.
    let package_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(node_join(&[&root, "package.json"])).unwrap()).unwrap();
    assert_eq!(package_json, json!({"name": "pi-extensions", "private": true}));
    assert_eq!(
        std::fs::read_to_string(node_join(&[&root, ".gitignore"])).unwrap(),
        "*\n!.gitignore\n"
    );
}

#[tokio::test]
async fn passes_legacy_peer_deps_when_uninstalling() {
    let _g = env_lock();
    let f = Fixture::new();
    let root = f.agent_path(&["npm"]);
    mkdir(&root);
    f.pm.remove("npm:@scope/pkg", None).await.unwrap();
    assert!(f.runner.ran(
        "npm",
        &["uninstall", "@scope/pkg", "--prefix", &root, "--legacy-peer-deps"],
        None
    ));
}

#[tokio::test]
async fn uses_bun_cwd_for_npm_package_installs() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["mise", "exec", "bun@1", "--", "bun"]}));
    f.pm.install("npm:@scope/pkg", None).await.unwrap();
    let root = f.agent_path(&["npm"]);
    assert!(f.runner.ran(
        "mise",
        &["exec", "bun@1", "--", "bun", "install", "@scope/pkg", "--cwd", &root, "--omit=peer"],
        None
    ));
}

fn clone_creates_package(f: &Fixture, target_dir: String) {
    f.runner.on_run(move |c| {
        if c.command == "git" && c.args[0] == "clone" {
            write(
                &node_join(&[&target_dir, "package.json"]),
                &json!({"name": "repo", "version": "1.0.0"}).to_string(),
            );
        }
        Ok(())
    });
}

#[tokio::test]
async fn installs_git_package_dependencies_without_auto_installing_peers() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    clone_creates_package(&f, target.clone());
    f.pm.install("git:github.com/user/repo", None).await.unwrap();
    assert!(f.runner.ran(
        "git",
        &["clone", "https://github.com/user/repo", &target],
        None
    ));
    assert!(f.runner.ran("npm", &["install", "--omit=dev", "--legacy-peer-deps"], Some(&target)));
    // ensureGitIgnore on the git root
    assert!(exists(&f.agent_path(&["git", ".gitignore"])));
}

#[tokio::test]
async fn removes_new_checkout_when_clone_or_dependency_install_fails() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    let t = target.clone();
    f.runner.on_run(move |c| {
        if c.command == "git" && c.args[0] == "clone" {
            mkdir(&t);
            bail!("simulated git clone failure");
        }
        Ok(())
    });
    let error = f.pm.install("git:github.com/user/repo", None).await.unwrap_err();
    assert!(error.to_string().contains("simulated git clone failure"));
    assert!(!exists(&target));

    let t = target.clone();
    f.runner.on_run(move |c| {
        if c.command == "git" && c.args[0] == "clone" {
            write(&node_join(&[&t, "package.json"]), "{\"name\":\"repo\"}");
            return Ok(());
        }
        if c.command == "npm" {
            bail!("simulated dependency install failure");
        }
        Ok(())
    });
    let error = f.pm.install("git:github.com/user/repo", None).await.unwrap_err();
    assert!(error.to_string().contains("simulated dependency install failure"));
    assert!(!exists(&target));
}

#[tokio::test]
async fn reconciles_existing_checkout_to_pinned_ref_during_install() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    write(&node_join(&[&target, "package.json"]), "{\"name\":\"repo\",\"version\":\"1.0.0\"}");
    f.runner.on_capture(|c| match (c.args[0].as_str(), c.args[1].as_str()) {
        ("rev-parse", "HEAD") => Ok("old-head".to_string()),
        ("rev-parse", "FETCH_HEAD^{commit}") => Ok("new-head".to_string()),
        _ => bail!("Unexpected runCommandCapture args: {}", c.args.join(" ")),
    });
    f.pm.install("git:github.com/user/repo@v2", None).await.unwrap();
    assert!(f.runner.ran("git", &["fetch", "origin", "v2"], Some(&target)));
    assert!(f.runner.ran("git", &["reset", "--hard", "FETCH_HEAD^{commit}"], Some(&target)));
    assert!(f.runner.ran("git", &["clean", "-fdx"], Some(&target)));
    assert!(f.runner.ran("npm", &["install", "--omit=dev", "--legacy-peer-deps"], Some(&target)));
    // The incomplete-update marker is removed after a successful reconcile.
    assert!(!exists(&f.agent_path(&["git", "github.com", "user", ".repo.pi-update-incomplete"])));
}

#[tokio::test]
async fn reconciles_existing_checkout_to_update_target_without_ref() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    mkdir(&target);
    f.runner.on_capture(|c| match c.args.join(" ").as_str() {
        "rev-parse --abbrev-ref @{upstream}" => bail!("no upstream"),
        "rev-parse origin/HEAD" => Ok("new-head".to_string()),
        "symbolic-ref refs/remotes/origin/HEAD" => Ok("refs/remotes/origin/main".to_string()),
        "rev-parse HEAD" => Ok("old-head".to_string()),
        "rev-parse origin/HEAD^{commit}" => Ok("new-head".to_string()),
        other => bail!("Unexpected runCommandCapture args: {other}"),
    });
    f.pm.install("git:github.com/user/repo", None).await.unwrap();
    assert!(f.runner.ran("git", &["remote", "set-head", "origin", "-a"], Some(&target)));
    assert!(f.runner.ran(
        "git",
        &["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"],
        Some(&target)
    ));
    assert!(f.runner.ran("git", &["reset", "--hard", "origin/HEAD^{commit}"], Some(&target)));
    assert!(f.runner.ran("git", &["clean", "-fdx"], Some(&target)));
}

#[tokio::test]
async fn detects_package_manager_names() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["npm", "exec", "--", "pnpm"]}));
    assert_eq!(f.pm.get_package_manager_name().unwrap(), "pnpm");
    assert_eq!(
        f.pm.get_git_dependency_install_args().unwrap(),
        s(&[
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ])
    );
    let f = Fixture::with_settings(json!({"npmCommand": ["C:\\tools\\bun.exe"]}));
    assert_eq!(f.pm.get_package_manager_name().unwrap(), if cfg!(windows) { "bun" } else { "C:\\tools\\bun" });
    let f = Fixture::with_settings(json!({"npmCommand": ["corepack", "pnpm", "npm"]}));
    assert_eq!(
        f.pm.get_package_manager_name().unwrap_err().to_string(),
        "Ambiguous npmCommand package managers: pnpm, npm"
    );
    let f = Fixture::with_settings(json!({"npmCommand": ["", "x"]}));
    assert_eq!(
        f.pm.get_package_manager_name().unwrap_err().to_string(),
        "Invalid npmCommand: first array entry must be a non-empty command"
    );
}

#[tokio::test]
async fn detects_pnpm_through_corepack_and_bun_for_git_dependencies() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["corepack", "pnpm"]}));
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    clone_creates_package(&f, target.clone());
    f.pm.install("git:github.com/user/repo", None).await.unwrap();
    assert!(f.runner.ran(
        "corepack",
        &[
            "pnpm",
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ],
        Some(&target)
    ));

    let f = Fixture::with_settings(json!({"npmCommand": ["bun"]}));
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    clone_creates_package(&f, target.clone());
    f.pm.install("git:github.com/user/repo", None).await.unwrap();
    assert!(f.runner.ran("bun", &["install", "--omit=dev", "--omit=peer"], Some(&target)));
}

fn upstream_capture(f: &Fixture) {
    f.runner.on_capture(|c| {
        let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["rev-parse", "--abbrev-ref", "@{upstream}"] => Ok("origin/main".to_string()),
            ["rev-parse", "@{upstream}"] | ["rev-parse", "@{upstream}^{commit}"] => Ok("remote-head".to_string()),
            ["rev-parse", "HEAD"] => Ok("local-head".to_string()),
            _ => bail!("Unexpected runCommandCapture args: {}", a.join(" ")),
        }
    });
}

#[tokio::test]
async fn updates_git_package_dependencies_without_auto_installing_peers() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.path(&[".pi", "git", "github.com", "user", "repo"]);
    write(&node_join(&[&target, "package.json"]), "{\"name\":\"repo\",\"version\":\"1.0.0\"}");
    f.settings
        .set_project_packages(&[pkg(json!("git:github.com/user/repo"))])
        .unwrap();
    upstream_capture(&f);
    f.pm.update(Some("git:github.com/user/repo")).await.unwrap();
    assert!(f.runner.ran(
        "git",
        &["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"],
        Some(&target)
    ));
    assert!(f.runner.ran("npm", &["install", "--omit=dev", "--legacy-peer-deps"], Some(&target)));

    let f = Fixture::with_settings(json!({"npmCommand": ["mise", "exec", "node@20", "--", "pnpm"]}));
    let target = f.path(&[".pi", "git", "github.com", "user", "repo"]);
    write(&node_join(&[&target, "package.json"]), "{\"name\":\"repo\",\"version\":\"1.0.0\"}");
    f.settings
        .set_project_packages(&[pkg(json!("git:github.com/user/repo"))])
        .unwrap();
    upstream_capture(&f);
    f.pm.update(Some("git:github.com/user/repo")).await.unwrap();
    assert!(f.runner.ran(
        "mise",
        &[
            "exec",
            "node@20",
            "--",
            "pnpm",
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ],
        Some(&target)
    ));
}

#[tokio::test]
async fn repairs_missing_git_dependencies_when_checkout_is_current() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    write(
        &node_join(&[&target, "package.json"]),
        &json!({"name": "repo", "version": "1.0.0", "dependencies": {"dependency": "1.0.0"}}).to_string(),
    );
    f.settings.set_packages(&[pkg(json!("git:github.com/user/repo"))]);
    f.runner.on_capture(|c| {
        if c.args.join(" ") == "rev-parse --abbrev-ref @{upstream}" {
            return Ok("origin/main".to_string());
        }
        Ok("current-head".to_string())
    });
    f.pm.update(Some("git:github.com/user/repo")).await.unwrap();
    assert!(f.runner.ran("npm", &["install", "--omit=dev", "--legacy-peer-deps"], Some(&target)));
    assert!(!f.runner.ran("git", &["clean", "-fdx"], Some(&target)));
}

#[tokio::test]
async fn repairs_deleted_git_dependencies_when_cleaning_fails() {
    let _g = env_lock();
    let f = Fixture::new();
    let target = f.agent_path(&["git", "github.com", "user", "repo"]);
    write(
        &node_join(&[&target, "package.json"]),
        &json!({"name": "repo", "version": "1.0.0", "dependencies": {"dependency": "1.0.0"}}).to_string(),
    );
    f.settings.set_packages(&[pkg(json!("git:github.com/user/repo"))]);
    f.runner.on_capture(|c| {
        if c.args.join(" ") == "rev-parse --abbrev-ref @{upstream}" {
            return Ok("origin/main".to_string());
        }
        Ok(if c.args[1] == "HEAD" { "old-head" } else { "new-head" }.to_string())
    });
    f.runner.on_run(|c| {
        if c.args[0] == "clean" {
            bail!("simulated clean failure");
        }
        Ok(())
    });
    let error = f.pm.update(Some("git:github.com/user/repo")).await.unwrap_err();
    assert!(error.to_string().contains("simulated clean failure"));
    assert!(f.runner.ran("npm", &["install", "--omit=dev", "--legacy-peer-deps"], Some(&target)));
    // The incomplete marker stays so the next update cleans again.
    assert!(exists(&f.agent_path(&["git", "github.com", "user", ".repo.pi-update-incomplete"])));
}

#[tokio::test]
async fn uses_npm_command_for_root_lookup_and_invalidates_cache() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["mise", "exec", "node@20", "--", "npm"]}));
    let root20 = f.path(&["node20", "lib", "node_modules"]);
    let root22 = f.path(&["node22", "lib", "node_modules"]);
    mkdir(&node_join(&[&root20, "@scope", "pkg"]));
    let (r20, r22) = (root20.clone(), root22.clone());
    f.runner.on_sync(move |c| {
        if c.command != "mise" {
            bail!("unexpected command {}", c.command);
        }
        match c.args[1].as_str() {
            "node@20" => Ok(r20.clone()),
            "node@22" => Ok(r22.clone()),
            _ => bail!("unexpected args"),
        }
    });
    assert_eq!(
        f.pm.get_installed_path("npm:@scope/pkg", SettingsScope::User).unwrap(),
        Some(node_join(&[&root20, "@scope", "pkg"]))
    );
    assert_eq!(f.runner.syncs()[0], call("mise", &["exec", "node@20", "--", "npm", "root", "-g"], None));
    f.settings.set_npm_command(Some(&s(&["mise", "exec", "node@22", "--", "npm"])));
    assert_eq!(f.pm.get_installed_path("npm:@scope/pkg", SettingsScope::User).unwrap(), None);
    assert_eq!(f.runner.syncs()[1], call("mise", &["exec", "node@22", "--", "npm", "root", "-g"], None));
}

#[tokio::test]
async fn installs_user_npm_packages_into_managed_root_once() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["pnpm"], "packages": ["npm:pnpm-pkg"]}));
    let package_path = f.agent_path(&["npm", "node_modules", "pnpm-pkg"]);
    let root = f.agent_path(&["npm"]);
    let (pp, r) = (package_path.clone(), root.clone());
    f.runner.on_run(move |c| {
        assert_eq!(c.command, "pnpm");
        assert_eq!(
            c.args,
            s(&[
                "install",
                "pnpm-pkg",
                "--prefix",
                &r,
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ])
        );
        write(&node_join(&[&pp, "package.json"]), "{\"name\":\"pnpm-pkg\",\"version\":\"1.0.0\"}");
        write(&node_join(&[&pp, "extensions", "index.ts"]), EXT);
        Ok(())
    });
    let index = node_join(&[&package_path, "extensions", "index.ts"]);
    let first = f.pm.resolve(None).await.unwrap();
    let second = f.pm.resolve(None).await.unwrap();
    assert!(first.extensions.iter().any(|r| r.path == index && r.enabled));
    assert!(second.extensions.iter().any(|r| r.path == index && r.enabled));
    assert_eq!(f.runner.runs().len(), 1);
    assert_eq!(
        f.pm.get_installed_path("npm:pnpm-pkg", SettingsScope::User).unwrap(),
        Some(package_path)
    );
    let metadata = &first.extensions.iter().find(|r| r.path == index).unwrap().metadata;
    assert_eq!(metadata.origin, SourceOrigin::Package);
    assert_eq!(metadata.source, "npm:pnpm-pkg");
}

#[tokio::test]
async fn loads_legacy_pnpm_global_package_paths() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["pnpm"], "packages": ["npm:pnpm-pkg"]}));
    let pnpm_root = f.path(&["pnpm", "global", "v11"]);
    let package_path = node_join(&[&pnpm_root, "20-hash", "node_modules", "pnpm-pkg"]);
    write(&node_join(&[&package_path, "package.json"]), "{\"name\":\"pnpm-pkg\",\"version\":\"1.0.0\"}");
    write(&node_join(&[&package_path, "extensions", "index.ts"]), EXT);
    let (pr, pp) = (pnpm_root.clone(), package_path.clone());
    f.runner.on_sync(move |c| {
        if c.command != "pnpm" {
            bail!("unexpected command {}", c.command);
        }
        if c.args.join(" ") == "list -g --depth 0 --json" {
            return Ok(json!([{"path": pr, "dependencies": {"pnpm-pkg": {"version": "1.0.0", "path": pp}}}]).to_string());
        }
        bail!("unexpected args {}", c.args.join(" "))
    });
    let result = f.pm.resolve(None).await.unwrap();
    let index = node_join(&[&package_path, "extensions", "index.ts"]);
    assert!(result.extensions.iter().any(|r| r.path == index && r.enabled));
    assert!(f.runner.runs().is_empty());
    assert_eq!(
        f.pm.get_installed_path("npm:pnpm-pkg", SettingsScope::User).unwrap(),
        Some(package_path)
    );

    let f = Fixture::with_settings(json!({"npmCommand": ["pnpm"]}));
    f.runner.on_sync(|_| Ok("not json".to_string()));
    assert_eq!(f.pm.get_installed_path("npm:pnpm-pkg", SettingsScope::User).unwrap(), None);
}

// ---------------------------------------------------------------------------
// source parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn emits_progress_events_on_install_attempts() {
    let _g = env_lock();
    let f = Fixture::new();
    let events = Arc::new(StdMutex::new(Vec::<ProgressEvent>::new()));
    let sink = events.clone();
    let cb: ProgressCallback = Arc::new(move |e| sink.lock().unwrap().push(e));
    f.pm.set_progress_callback(Some(&cb));
    f.runner.on_run(|_| bail!("simulated npm install failure"));
    let error = f.pm.install("npm:nonexistent-package@1.0.0", None).await.unwrap_err();
    assert_eq!(error.to_string(), "simulated npm install failure");
    {
        let events = events.lock().unwrap();
        assert!(events.iter().any(|e| e.r#type == ProgressEventType::Start
            && e.action == ProgressEventAction::Install
            && e.message.as_deref() == Some("Installing npm:nonexistent-package@1.0.0...")));
        assert!(events.iter().any(|e| e.r#type == ProgressEventType::Error
            && e.message.as_deref() == Some("simulated npm install failure")));
    }

    f.runner.on_run(|_| bail!("simulated git clone failure"));
    let source = "https://github.com/nonexistent/repo";
    let error = f.pm.install(source, None).await.unwrap_err();
    assert_eq!(error.to_string(), "simulated git clone failure");
    let last = f.runner.runs().last().cloned().unwrap();
    assert_eq!(last.command, "git");
    assert_eq!(last.args[..2], s(&["clone", source])[..]);
}

#[test]
fn parses_package_source_types_from_docs_examples() {
    let f = Fixture::new();
    assert!(npm(&f.pm, "npm:@scope/pkg@1.2.3").pinned);
    assert!(!npm(&f.pm, "npm:@scope/pkg@^1.2.3").pinned);
    assert!(!npm(&f.pm, "npm:pkg").pinned);
    let parsed = npm(&f.pm, "npm:@scope/pkg@^1.2.3");
    assert_eq!(parsed.name, "@scope/pkg");
    assert_eq!(parsed.version.as_deref(), Some("^1.2.3"));
    // `semver.validRange` normalizes the range.
    assert_eq!(parsed.range.as_deref(), Some(">=1.2.3 <2.0.0-0"));
    let latest = npm(&f.pm, "npm:pkg@latest");
    assert_eq!((latest.range, latest.pinned), (None, false));

    for source in [
        "git:github.com/user/repo@v1",
        "https://github.com/user/repo@v1",
        "git:git@github.com:user/repo@v1",
        "ssh://git@github.com/user/repo@v1",
    ] {
        git(&f.pm, source);
    }
    for source in [
        "/absolute/path/to/package",
        "./relative/path/to/package",
        "../relative/path/to/package",
        "github.com/user/repo",
        "git@github.com:user/repo",
    ] {
        assert!(is_local(&f.pm, source), "{source}");
    }
    match f.pm.parse_source("./packages/agent-timers") {
        ParsedSource::LocalSource(l) => assert_eq!(l.path, "./packages/agent-timers"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_ssh_and_https_git_sources() {
    let f = Fixture::new();
    let parsed = git(&f.pm, "ssh://git@github.com/user/repo");
    assert_eq!((parsed.host.as_str(), parsed.path.as_str()), ("github.com", "user/repo"));
    assert_eq!(parsed.repo, "ssh://git@github.com/user/repo");
    let parsed = git(&f.pm, "git:git@github.com:user/repo");
    assert_eq!(parsed.repo, "git@github.com:user/repo");
    assert!(!parsed.pinned);
    let parsed = git(&f.pm, "git:git@github.com:user/repo@v1.0.0");
    assert_eq!(parsed.r#ref.as_deref(), Some("v1.0.0"));
    assert!(parsed.pinned);
    let parsed = git(&f.pm, "https://github.com/user/repo@v1.2.3");
    assert_eq!(parsed.r#ref.as_deref(), Some("v1.2.3"));
    let parsed = git(&f.pm, "https://github.com/user/repo@feature/branch");
    assert_eq!(parsed.r#ref.as_deref(), Some("feature/branch"));
    for (source, host) in [
        ("https://gitlab.com/user/repo", "gitlab.com"),
        ("https://bitbucket.org/user/repo", "bitbucket.org"),
        ("https://codeberg.org/user/repo", "codeberg.org"),
        ("https://github.com/user/repo.git", "github.com"),
        ("git:https://github.com/user/repo", "github.com"),
    ] {
        let parsed = git(&f.pm, source);
        assert_eq!((parsed.host.as_str(), parsed.path.as_str()), (host, "user/repo"));
    }
}

#[test]
fn normalizes_git_package_identities() {
    let f = Fixture::new();
    for source in [
        "https://github.com/user/repo",
        "https://github.com/user/repo@v1.0.0",
        "git:github.com/user/repo",
        "https://github.com/user/repo.git",
        "git:git@github.com:user/repo",
        "ssh://git@github.com/user/repo",
    ] {
        assert_eq!(
            f.pm.get_package_identity(source, None).unwrap(),
            "git:github.com/user/repo",
            "{source}"
        );
    }
}

#[test]
fn rejects_git_paths_outside_install_roots() {
    let f = Fixture::new();
    let traversal = GitSource {
        r#type: "git".to_string(),
        repo: "git@evil.example:../../victim/repo".to_string(),
        host: "evil.example".to_string(),
        path: "../../victim/repo".to_string(),
        r#ref: None,
        pinned: false,
    };
    for scope in [SourceScope::User, SourceScope::Project, SourceScope::Temporary] {
        let error = f.pm.get_git_install_path(&traversal, scope).unwrap_err();
        assert!(error.to_string().contains("outside package install root"), "{error}");
    }
}

#[test]
fn places_temporary_npm_packages_under_agent_temp_folder() {
    let f = Fixture::new();
    let source = npm(&f.pm, "npm:left-pad");
    let install_path = f.pm.get_npm_install_path(&source, SourceScope::Temporary).unwrap();
    let temp_root = f.agent_path(&["tmp", "extensions"]);
    assert!(ends_with(&install_path, "node_modules/left-pad"));
    assert!(!node_relative(&temp_root, &install_path).starts_with(".."));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&temp_root).unwrap().permissions().mode() & 0o777, 0o700);
    }
    // Pinned temporary git refs get their own checkout.
    let a = f.pm.get_git_install_path(&git(&f.pm, "git:github.com/example/repo@aaaaaaa"), SourceScope::Temporary).unwrap();
    let b = f.pm.get_git_install_path(&git(&f.pm, "git:github.com/example/repo@bbbbbbb"), SourceScope::Temporary).unwrap();
    assert_ne!(a, b);
    assert!(ends_with(&a, "example/repo"));
}

// ---------------------------------------------------------------------------
// settings source normalization
// ---------------------------------------------------------------------------

#[test]
fn stores_local_packages_relative_to_settings_base() {
    let f = Fixture::new();
    let pkg_dir = f.path(&["packages", "local-global-pkg"]);
    write(&node_join(&[&pkg_dir, "extensions", "index.ts"]), EXT);
    // Local paths are resolved against the process cwd for input; use an absolute path.
    assert!(f.pm.add_source_to_settings(&pkg_dir, None).unwrap());
    let stored = package_source_string(&f.settings.get_global_settings().packages.unwrap()[0]).to_string();
    assert_eq!(stored, node_relative(&f.agent, &pkg_dir));

    let project_pkg = f.path(&["project-local-pkg"]);
    write(&node_join(&[&project_pkg, "extensions", "index.ts"]), EXT);
    assert!(f
        .pm
        .add_source_to_settings(&project_pkg, Some(&PackageManagerAddSourceToSettingsOptions { local: Some(true) }))
        .unwrap());
    let stored = package_source_string(&f.settings.get_project_settings().packages.unwrap()[0]).to_string();
    assert_eq!(stored, "../project-local-pkg");
}

#[test]
fn removes_local_package_entries_using_equivalent_path_forms() {
    let f = Fixture::new();
    let pkg_dir = f.path(&["remove-local-pkg"]);
    write(&node_join(&[&pkg_dir, "extensions", "index.ts"]), EXT);
    f.pm.add_source_to_settings(&pkg_dir, None).unwrap();
    assert!(f.pm.remove_source_from_settings(&format!("{pkg_dir}/"), None).unwrap());
    assert!(f.settings.get_global_settings().packages.unwrap_or_default().is_empty());
    assert!(!f.pm.remove_source_from_settings(&pkg_dir, None).unwrap());
}

#[test]
fn updates_git_refs_and_preserves_filters_in_settings() {
    let f = Fixture::new();
    assert!(f.pm.add_source_to_settings("git:github.com/user/repo@v1", None).unwrap());
    assert!(!f.pm.add_source_to_settings("git:github.com/user/repo@v1", None).unwrap());
    let packages = |f: &Fixture| serde_json::to_value(f.settings.get_global_settings().packages).unwrap();
    assert_eq!(packages(&f), json!(["git:github.com/user/repo@v1"]));
    assert!(f.pm.add_source_to_settings("git:github.com/user/repo@v2", None).unwrap());
    assert_eq!(packages(&f), json!(["git:github.com/user/repo@v2"]));

    let f = Fixture::new();
    f.settings.set_packages(&[pkg(json!({
        "source": "git:github.com/user/repo@v1",
        "extensions": ["extensions/main.ts"],
        "skills": [],
        "prompts": ["prompts/review.md"],
        "themes": ["themes/dark.json"],
    }))]);
    assert!(f.pm.add_source_to_settings("git:github.com/user/repo@v2", None).unwrap());
    assert_eq!(
        packages(&f),
        json!([{
            "source": "git:github.com/user/repo@v2",
            "extensions": ["extensions/main.ts"],
            "skills": [],
            "prompts": ["prompts/review.md"],
            "themes": ["themes/dark.json"],
        }])
    );
}

#[test]
fn refuses_project_package_storage_when_untrusted() {
    let f = Fixture::new();
    f.settings.set_project_trusted(false);
    let error = f
        .pm
        .add_source_to_settings("./x", Some(&PackageManagerAddSourceToSettingsOptions { local: Some(true) }))
        .unwrap_err();
    assert_eq!(error.to_string(), "Project is not trusted; refusing to access project package storage");
}

// ---------------------------------------------------------------------------
// pattern filtering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expands_tilde_in_exclusion_and_exact_patterns() {
    let _g = env_lock();
    let f = Fixture::new();
    let previous_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", &f.temp) };
    let ext_dir = f.path(&["home-exts"]);
    write(&node_join(&[&ext_dir, "a.ts"]), EXT);
    write(&node_join(&[&ext_dir, "b.ts"]), EXT);
    write(&node_join(&[&ext_dir, "c.ts"]), EXT);
    f.settings
        .set_extension_paths(&s(&["~/home-exts", "!~/home-exts/a.ts", "-~/home-exts/b.ts"]));
    let result = f.pm.resolve(None).await;
    match previous_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let result = result.unwrap();
    assert!(result.extensions.iter().any(|r| is_disabled(r, "home-exts/a.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "home-exts/b.ts")));
    assert!(result.extensions.iter().any(|r| is_enabled(r, "home-exts/c.ts")));
}

#[tokio::test]
async fn filters_top_level_arrays_with_patterns() {
    let _g = env_lock();
    let f = Fixture::new();
    for name in ["keep.ts", "legacy.ts"] {
        write(&f.agent_path(&["exts", name]), EXT);
    }
    for name in ["dark.json", "light.json", "funky.json"] {
        write(&f.agent_path(&["themes-src", name]), "{}");
    }
    for name in ["review.md", "draft.md"] {
        write(&f.agent_path(&["prompts-src", name]), "p");
    }
    write(&f.agent_path(&["skills-src", "good", "SKILL.md"]), "---\nname: good\ndescription: d\n---\n");
    write(&f.agent_path(&["skills-src", "bad", "SKILL.md"]), "---\nname: bad\ndescription: d\n---\n");
    f.settings.set_extension_paths(&s(&["exts", "!exts/legacy.ts"]));
    f.settings.set_theme_paths(&s(&["themes-src", "*.json", "!funky.json"]));
    f.settings.set_prompt_template_paths(&s(&["prompts-src", "!draft.md"]));
    f.settings.set_skill_paths(&s(&["skills-src", "!bad"]));
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "exts/keep.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "exts/legacy.ts")));
    assert!(result.themes.iter().any(|r| is_enabled(r, "dark.json")));
    assert!(result.themes.iter().any(|r| is_disabled(r, "funky.json")));
    assert!(result.prompts.iter().any(|r| is_enabled(r, "review.md")));
    assert!(result.prompts.iter().any(|r| is_disabled(r, "draft.md")));
    assert!(result.skills.iter().any(|r| is_enabled(r, "good/SKILL.md")));
    assert!(result.skills.iter().any(|r| is_disabled(r, "bad/SKILL.md")));
}

#[tokio::test]
async fn force_include_and_force_exclude_top_level() {
    let _g = env_lock();
    let f = Fixture::new();
    for name in ["a.ts", "b.ts", "c.ts"] {
        write(&f.agent_path(&["exts", name]), EXT);
    }
    f.settings
        .set_extension_paths(&s(&["exts", "!exts/*.ts", "+exts/a.ts", "+exts/b.ts", "-exts/b.ts"]));
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "exts/a.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "exts/b.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "exts/c.ts")));
}

#[tokio::test]
async fn supports_glob_patterns_in_manifests() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["manifest-pkg"]);
    write(&node_join(&[&pkg_dir, "extensions", "local.ts"]), EXT);
    write(&node_join(&[&pkg_dir, "node_modules/dep/extensions", "remote.ts"]), EXT);
    write(&node_join(&[&pkg_dir, "node_modules/dep/extensions", "skip.ts"]), EXT);
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "manifest-pkg", "pi": {"extensions": ["extensions", "node_modules/dep/extensions", "!**/skip.ts"]}}).to_string(),
    );
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "local.ts")));
    assert!(result.extensions.iter().any(|r| is_enabled(r, "remote.ts")));
    assert!(!result.extensions.iter().any(|r| ends_with(&r.path, "skip.ts")));

    let pkg_dir = f.path(&["skill-manifest-pkg"]);
    write(&node_join(&[&pkg_dir, "skills/good-skill", "SKILL.md"]), "---\nname: good-skill\ndescription: Good\n---\n");
    write(&node_join(&[&pkg_dir, "skills/bad-skill", "SKILL.md"]), "---\nname: bad-skill\ndescription: Bad\n---\n");
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "s", "pi": {"skills": ["skills", "!**/bad-skill"]}}).to_string(),
    );
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.skills.iter().any(|r| r.path.contains("good-skill") && r.enabled));
    assert!(!result.skills.iter().any(|r| r.path.contains("bad-skill")));

    let pkg_dir = f.path(&["skill-manifest-glob-pkg"]);
    write(&node_join(&[&pkg_dir, "plugins/pdf-to-markdown/skills/pdf-to-markdown", "SKILL.md"]), "---\nname: pdf\ndescription: d\n---\n");
    write(&node_join(&[&pkg_dir, "plugins/nutrient-dws/skills/document-processor-api", "SKILL.md"]), "---\nname: dws\ndescription: d\n---\n");
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "g", "pi": {"skills": ["./plugins/*/skills"]}}).to_string(),
    );
    let result = f.pm.resolve_extension_sources(&[pkg_dir], None).await.unwrap();
    assert!(result.skills.iter().any(|r| r.path.contains("pdf-to-markdown") && r.enabled));
    assert!(result.skills.iter().any(|r| r.path.contains("document-processor-api") && r.enabled));
}

#[cfg(unix)]
#[tokio::test]
async fn sorts_manifest_glob_matches_and_uses_exact_entries_for_dot_paths_and_symlinks() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["manifest-glob-semantics-pkg"]);
    let files = node_join(&[&pkg_dir, "extension-files"]);
    write(&node_join(&[&files, "z.ts"]), EXT);
    write(&node_join(&[&files, "a.ts"]), EXT);
    write(&node_join(&[&files, ".ignored.ts"]), EXT);
    write(&node_join(&[&files, "nested", ".hidden.ts"]), EXT);
    write(&node_join(&[&pkg_dir, "extension-groups", "group", "index.ts"]), EXT);
    write(&node_join(&[&pkg_dir, "plugins", "local", "skills", "local-skill", "SKILL.md"]), "---\nname: local-skill\ndescription: Local\n---\n");
    let linked = node_join(&[&pkg_dir, "linked-plugin-source"]);
    write(&node_join(&[&linked, "skills", "linked-skill", "SKILL.md"]), "---\nname: linked-skill\ndescription: Linked\n---\n");
    std::os::unix::fs::symlink(&linked, node_join(&[&pkg_dir, "plugins", "linked"])).unwrap();
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "m", "pi": {
            "extensions": ["./extension-files/*.ts", "./extension-files/**/.ignored.ts", "./extension-files/nested/.hidden.ts", "./extension-groups/*/"],
            "skills": ["./plugins/*/skills", "./plugins/linked/skills"],
        }})
        .to_string(),
    );
    let result = f.pm.resolve_extension_sources(std::slice::from_ref(&pkg_dir), None).await.unwrap();
    let rel: Vec<String> = result.extensions.iter().map(|r| node_relative(&pkg_dir, &r.path)).collect();
    assert_eq!(
        rel,
        s(&[
            "extension-files/a.ts",
            "extension-files/z.ts",
            "extension-files/nested/.hidden.ts",
            "extension-groups/group/index.ts",
        ])
    );
    assert!(result.skills.iter().any(|r| ends_with(&r.path, "local-skill/SKILL.md")));
    assert!(result.skills.iter().any(|r| ends_with(&r.path, "linked-skill/SKILL.md")));
}

fn layered_pkg(f: &Fixture) -> String {
    let pkg_dir = f.path(&["layered-pkg"]);
    for name in ["foo.ts", "bar.ts", "baz.ts"] {
        write(&node_join(&[&pkg_dir, "extensions", name]), EXT);
    }
    for name in ["dark.json", "light.json"] {
        write(&node_join(&[&pkg_dir, "themes", name]), "{}");
    }
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &json!({"name": "layered-pkg", "pi": {"extensions": ["extensions", "!**/baz.ts"], "themes": ["themes"]}}).to_string(),
    );
    pkg_dir
}

#[tokio::test]
async fn applies_user_filters_on_top_of_manifest_filters() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = layered_pkg(&f);
    f.settings.set_packages(&[pkg(json!({"source": pkg_dir, "extensions": ["!**/bar.ts"], "themes": ["+themes/dark.json", "!*.json"]}))]);
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "foo.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "bar.ts")));
    assert!(!result.extensions.iter().any(|r| ends_with(&r.path, "baz.ts")));
    assert!(result.themes.iter().any(|r| is_enabled(r, "dark.json")));
    assert!(result.themes.iter().any(|r| is_disabled(r, "light.json")));
    assert!(result.extensions.iter().all(|r| r.metadata.origin == SourceOrigin::Package));
}

#[tokio::test]
async fn empty_filter_arrays_disable_and_direct_paths_select() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = layered_pkg(&f);
    f.settings.set_packages(&[pkg(json!({"source": pkg_dir, "extensions": ["extensions/foo.ts"], "themes": []}))]);
    let result = f.pm.resolve(None).await.unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "foo.ts")));
    assert!(result.extensions.iter().any(|r| is_disabled(r, "bar.ts")));
    assert!(result.themes.iter().all(|r| !r.enabled));
    assert_eq!(result.themes.len(), 2);
}

#[tokio::test]
async fn resolves_autoload_disabled_project_entries_as_deltas() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = layered_pkg(&f);
    f.settings.set_packages(&[pkg(json!(pkg_dir))]);
    f.settings
        .set_project_packages(&[pkg(json!({"source": pkg_dir, "autoload": false, "extensions": ["-extensions/foo.ts"]}))])
        .unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    let foo: Vec<_> = result.extensions.iter().filter(|r| ends_with(&r.path, "foo.ts")).collect();
    assert_eq!(foo.len(), 1);
    assert!(!foo[0].enabled);
    assert_eq!(foo[0].metadata.scope, SourceScope::Project);
    let bar = result.extensions.iter().find(|r| ends_with(&r.path, "bar.ts")).unwrap();
    assert!(bar.enabled);
    assert_eq!(bar.metadata.scope, SourceScope::User);

    // Without a global entry the delta is positive-only.
    let f = Fixture::new();
    let pkg_dir = layered_pkg(&f);
    f.settings
        .set_project_packages(&[pkg(json!({"source": pkg_dir, "autoload": false, "extensions": ["extensions/bar.ts"]}))])
        .unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    assert_eq!(result.extensions.len(), 1);
    assert!(is_enabled(&result.extensions[0], "bar.ts"));
}

// ---------------------------------------------------------------------------
// package deduplication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dedupes_same_local_package_project_wins() {
    let _g = env_lock();
    let f = Fixture::new();
    let pkg_dir = f.path(&["shared-pkg"]);
    write(&node_join(&[&pkg_dir, "extensions", "shared.ts"]), EXT);
    let other = f.path(&["other-pkg"]);
    write(&node_join(&[&other, "extensions", "other.ts"]), EXT);
    f.settings.set_packages(&[pkg(json!(pkg_dir)), pkg(json!(other))]);
    f.settings.set_project_packages(&[pkg(json!(pkg_dir))]).unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    let shared: Vec<_> = result.extensions.iter().filter(|r| ends_with(&r.path, "shared.ts")).collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].metadata.scope, SourceScope::Project);
    assert!(result.extensions.iter().any(|r| ends_with(&r.path, "other.ts")));
}

#[test]
fn dedupes_ssh_and_https_urls_for_same_repo() {
    let f = Fixture::new();
    let packages = vec![
        (pkg(json!("git:git@github.com:user/repo")), SourceScope::Project),
        (pkg(json!("https://github.com/user/repo@v1")), SourceScope::User),
        (pkg(json!("ssh://git@github.com/user/repo")), SourceScope::User),
        (pkg(json!("git:github.com/user/repo")), SourceScope::User),
        (pkg(json!("https://github.com/user/other")), SourceScope::User),
    ];
    let deduped = f.pm.dedupe_packages(packages).unwrap();
    assert_eq!(deduped.len(), 2);
    assert_eq!(package_source_string(&deduped[0].0), "git:git@github.com:user/repo");
    assert_eq!(deduped[0].1, SourceScope::Project);
}

// ---------------------------------------------------------------------------
// multi-file extension discovery (issue #1102)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_file_extension_discovery() {
    let _g = env_lock();
    let f = Fixture::new();
    let ext = f.agent_path(&["extensions"]);
    write(&node_join(&[&ext, "top.ts"]), EXT);
    write(&node_join(&[&ext, "sub", "index.ts"]), EXT);
    write(&node_join(&[&ext, "sub", "helper.ts"]), "export const x = 1;");
    write(&node_join(&[&ext, "manifest", "package.json"]), &json!({"pi": {"extensions": ["./main.ts"]}}).to_string());
    write(&node_join(&[&ext, "manifest", "main.ts"]), EXT);
    write(&node_join(&[&ext, "manifest", "index.ts"]), EXT);
    write(&node_join(&[&ext, "plain", "util.ts"]), EXT);
    let result = f.pm.resolve(None).await.unwrap();
    let names: HashSet<String> = result
        .extensions
        .iter()
        .map(|r| node_relative(&ext, &r.path))
        .collect();
    assert_eq!(
        names,
        ["top.ts", "sub/index.ts", "manifest/main.ts"].iter().map(|s| s.to_string()).collect()
    );
}

// ---------------------------------------------------------------------------
// offline mode, updates and network timeouts
// ---------------------------------------------------------------------------

fn write_installed(path: &str, name: &str, version: &str) {
    write(&node_join(&[path, "package.json"]), &json!({"name": name, "version": version}).to_string());
}

#[tokio::test]
async fn updates_npm_range_packages_using_configured_spec() {
    let _g = env_lock();
    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "1.0.0");
    f.settings.set_project_packages(&[pkg(json!("npm:example@^1.0.0"))]).unwrap();
    f.runner.on_capture(|_| Ok("[\"1.0.0\",\"1.2.0\"]".to_string()));
    f.pm.update(Some("npm:example")).await.unwrap();
    assert_eq!(
        f.runner.captures(),
        vec![call("npm", &["view", "example@^1.0.0", "version", "--json"], Some(&f.temp))]
    );
    let root = f.path(&[".pi", "npm"]);
    assert!(f.runner.ran("npm", &["install", "example@^1.0.0", "--prefix", &root, "--legacy-peer-deps"], None));
}

#[tokio::test]
async fn skips_npm_updates_when_current_or_newer() {
    let _g = env_lock();
    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "1.3.1");
    f.settings.set_project_packages(&[pkg(json!("npm:example@^1.0.0"))]).unwrap();
    f.runner.on_capture(|_| Ok("[\"1.0.0\",\"1.3.1\",\"1.0.2\"]".to_string()));
    f.pm.update(Some("npm:example")).await.unwrap();
    assert!(f.runner.runs().is_empty());

    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "2.0.0");
    f.settings.set_project_packages(&[pkg(json!("npm:example"))]).unwrap();
    f.runner.on_capture(|_| Ok("\"1.9.0\"".to_string()));
    f.pm.update(Some("npm:example")).await.unwrap();
    assert_eq!(f.runner.captures(), vec![call("npm", &["view", "example", "version", "--json"], Some(&f.temp))]);
    assert!(f.runner.runs().is_empty());
}

#[tokio::test]
async fn migrates_legacy_user_npm_installs_into_managed_root() {
    let _g = env_lock();
    let f = Fixture::new();
    let legacy_root = f.path(&["legacy-global", "node_modules"]);
    let legacy_path = node_join(&[&legacy_root, "legacy-pkg"]);
    let managed_path = f.agent_path(&["npm", "node_modules", "legacy-pkg"]);
    write_installed(&legacy_path, "legacy-pkg", "1.0.0");
    f.settings.set_packages(&[pkg(json!("npm:legacy-pkg"))]);
    let lr = legacy_root.clone();
    f.runner.on_sync(move |c| {
        if c.args == s(&["root", "-g"]) {
            return Ok(lr.clone());
        }
        bail!("unexpected")
    });
    f.runner.on_capture(|_| Ok("\"1.0.0\"".to_string()));
    let (mp, root) = (managed_path.clone(), f.agent_path(&["npm"]));
    f.runner.on_run(move |c| {
        assert_eq!(c.command, "npm");
        assert_eq!(c.args, s(&["install", "legacy-pkg@latest", "--prefix", &root, "--legacy-peer-deps"]));
        write_installed(&mp, "legacy-pkg", "1.0.0");
        Ok(())
    });
    assert_eq!(f.pm.get_installed_path("npm:legacy-pkg", SettingsScope::User).unwrap(), Some(legacy_path));
    f.pm.update(Some("npm:legacy-pkg")).await.unwrap();
    assert!(f.runner.captures().is_empty());
    assert_eq!(f.runner.runs().len(), 1);
    assert_eq!(f.pm.get_installed_path("npm:legacy-pkg", SettingsScope::User).unwrap(), Some(managed_path));
}

#[tokio::test]
async fn batches_npm_updates_per_scope_and_runs_git_updates_in_parallel() {
    let _g = env_lock();
    let f = Fixture::new();
    for (scope_root, name) in [
        (f.agent_path(&["npm", "node_modules"]), "user-old"),
        (f.agent_path(&["npm", "node_modules"]), "user-current"),
        (f.agent_path(&["npm", "node_modules"]), "user-unknown"),
        (f.path(&[".pi", "npm", "node_modules"]), "project-old"),
        (f.path(&[".pi", "npm", "node_modules"]), "project-current"),
    ] {
        write_installed(&node_join(&[&scope_root, name]), name, "1.0.0");
    }
    f.settings.set_packages(
        &[
            "npm:user-old",
            "npm:user-current",
            "npm:user-unknown",
            "npm:user-pinned@1.0.0",
            "git:github.com/example/user-repo-a",
            "git:github.com/example/user-repo-b",
            "git:github.com/example/user-repo-pinned@v1",
        ]
        .map(|p| pkg(json!(p))),
    );
    f.settings
        .set_project_packages(
            &[
                "npm:project-old",
                "npm:project-current",
                "npm:project-missing",
                "git:github.com/example/project-repo-a",
            ]
            .map(|p| pkg(json!(p))),
        )
        .unwrap();
    // Existing git checkouts whose update is simulated with delays.
    for repo in ["user-repo-a", "user-repo-b", "user-repo-pinned"] {
        mkdir(&f.agent_path(&["git", "github.com", "example", repo]));
    }
    mkdir(&f.path(&[".pi", "git", "github.com", "example", "project-repo-a"]));

    let active_git = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_git = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (ag, mg) = (active_git.clone(), max_git.clone());
    let capture: CaptureFn = Box::new(move |c| {
        let args = c.args.clone();
        let (ag, mg) = (ag.clone(), mg.clone());
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            if args[0] == "view" {
                return match args[1].as_str() {
                    "user-old" | "project-old" => Ok("\"2.0.0\"".to_string()),
                    "user-current" | "project-current" => Ok("\"1.0.0\"".to_string()),
                    "user-unknown" => bail!("registry unavailable"),
                    other => bail!("Unexpected package lookup: {other}"),
                };
            }
            match args.join(" ").as_str() {
                "rev-parse --abbrev-ref @{upstream}" => {
                    let n = ag.fetch_add(1, SeqCst) + 1;
                    mg.fetch_max(n, SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    ag.fetch_sub(1, SeqCst);
                    Ok("origin/main".to_string())
                }
                _ => Ok("same-head".to_string()),
            }
        })
    });
    *f.runner.capture_fn.lock().unwrap() = Some(Arc::new(capture));
    let active_npm = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_npm = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (an, mn) = (active_npm.clone(), max_npm.clone());
    f.runner.on_run_async(Box::new(move |c| {
        let c = c.clone();
        let (an, mn) = (an.clone(), mn.clone());
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            if c.command == "npm" {
                let n = an.fetch_add(1, SeqCst) + 1;
                mn.fetch_max(n, SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                an.fetch_sub(1, SeqCst);
            }
            Ok(())
        })
    }));

    f.pm.update(None).await.unwrap();

    let views: Vec<Call> = f.runner.captures().into_iter().filter(|c| c.args[0] == "view").collect();
    assert_eq!(views.len(), 5);
    let npm_runs: Vec<Call> = f.runner.runs().into_iter().filter(|c| c.command == "npm").collect();
    assert_eq!(npm_runs.len(), 2);
    let user_root = f.agent_path(&["npm"]);
    let project_root = f.path(&[".pi", "npm"]);
    assert!(npm_runs.contains(&call(
        "npm",
        &["install", "user-old@latest", "user-unknown@latest", "--prefix", &user_root, "--legacy-peer-deps"],
        None
    )));
    assert!(npm_runs.contains(&call(
        "npm",
        &["install", "project-old@latest", "project-missing@latest", "--prefix", &project_root, "--legacy-peer-deps"],
        None
    )));
    // 3 unpinned git checkouts reconcile against their upstream (pinned one fetches its ref).
    let fetches: Vec<Call> = f.runner.runs().into_iter().filter(|c| c.command == "git" && c.args[0] == "fetch").collect();
    assert_eq!(fetches.len(), 4);
    assert!(max_npm.load(std::sync::atomic::Ordering::SeqCst) > 1);
    assert!(max_git.load(std::sync::atomic::Ordering::SeqCst) > 1);
}

#[tokio::test]
async fn suggests_source_prefixes_for_update_lookups() {
    let _g = env_lock();
    let f = Fixture::new();
    f.settings.set_project_packages(&[pkg(json!("npm:example"))]).unwrap();
    assert_eq!(
        f.pm.update(Some("example")).await.unwrap_err().to_string(),
        "No matching package found for example. Did you mean npm:example?"
    );
    let f = Fixture::new();
    f.settings.set_project_packages(&[pkg(json!("git:github.com/example/repo"))]).unwrap();
    assert_eq!(
        f.pm.update(Some("github.com/example/repo")).await.unwrap_err().to_string(),
        "No matching package found for github.com/example/repo. Did you mean git:github.com/example/repo?"
    );
    assert_eq!(
        f.pm.update(Some("npm:nothing")).await.unwrap_err().to_string(),
        "No matching package found for npm:nothing"
    );
}

struct OfflineGuard;
impl OfflineGuard {
    fn set() -> Self {
        unsafe { std::env::set_var("PI_OFFLINE", "1") };
        OfflineGuard
    }
}
impl Drop for OfflineGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("PI_OFFLINE") };
    }
}

#[tokio::test]
async fn skips_installing_missing_sources_and_update_checks_when_offline() {
    let _g = env_lock();
    let _offline = OfflineGuard::set();
    let f = Fixture::new();
    f.settings
        .set_project_packages(&[pkg(json!("npm:missing-package")), pkg(json!("git:github.com/example/missing-repo"))])
        .unwrap();
    let result = f.pm.resolve(None).await.unwrap();
    let all: Vec<_> = result
        .extensions
        .iter()
        .chain(&result.skills)
        .chain(&result.prompts)
        .chain(&result.themes)
        .collect();
    assert!(!all.iter().any(|r| r.metadata.origin == SourceOrigin::Package));
    assert!(f.runner.runs().is_empty());
    assert!(f.pm.check_for_available_updates().await.unwrap().is_empty());
    assert!(f.runner.captures().is_empty());
    f.pm.update(None).await.unwrap();
    assert!(f.runner.runs().is_empty());
}

#[tokio::test]
async fn skips_refreshing_temporary_git_sources_when_offline() {
    let _g = env_lock();
    let _offline = OfflineGuard::set();
    let f = Fixture::new();
    let source = "git:github.com/example/repo";
    let installed = f.pm.get_git_install_path(&git(&f.pm, source), SourceScope::Temporary).unwrap();
    write(&node_join(&[&installed, "extensions", "index.ts"]), EXT);
    let result = f
        .pm
        .resolve_extension_sources(&s(&[source]), Some(&PackageManagerResolveExtensionSourcesOptions { local: None, temporary: Some(true) }))
        .await
        .unwrap();
    assert!(result.extensions.iter().any(|r| is_enabled(r, "extensions/index.ts")));
    assert!(f.runner.runs().is_empty() && f.runner.captures().is_empty());
}

#[tokio::test]
async fn loads_new_checkout_when_pinned_temporary_git_source_changes_ref() {
    let _g = env_lock();
    let f = Fixture::new();
    let old_path = f.pm.get_git_install_path(&git(&f.pm, "git:github.com/example/repo@aaaaaaa"), SourceScope::Temporary).unwrap();
    write(&node_join(&[&old_path, "extensions", "old.ts"]), EXT);
    let new_path = f.pm.get_git_install_path(&git(&f.pm, "git:github.com/example/repo@bbbbbbb"), SourceScope::Temporary).unwrap();
    let np = new_path.clone();
    f.runner.on_run(move |c| {
        if c.args[0] == "clone" {
            write(&node_join(&[&np, "extensions", "new.ts"]), EXT);
        }
        Ok(())
    });
    let result = f
        .pm
        .resolve_extension_sources(&s(&["git:github.com/example/repo@bbbbbbb"]), Some(&PackageManagerResolveExtensionSourcesOptions { local: None, temporary: Some(true) }))
        .await
        .unwrap();
    let clones: Vec<Call> = f.runner.runs().into_iter().filter(|c| c.args[0] == "clone").collect();
    assert_eq!(clones.len(), 1);
    assert!(f.runner.ran("git", &["checkout", "bbbbbbb"], Some(&new_path)));
    assert!(result.extensions.iter().any(|r| is_enabled(r, "extensions/new.ts")));
    assert!(!result.extensions.iter().any(|r| ends_with(&r.path, "extensions/old.ts")));
}

#[tokio::test]
async fn does_not_run_npm_view_during_resolve_and_reinstalls_mismatched_pins() {
    let _g = env_lock();
    {
        let _offline = OfflineGuard::set();
        let f = Fixture::new();
        let installed = f.path(&[".pi", "npm", "node_modules", "example"]);
        write_installed(&installed, "example", "1.0.0");
        write(&node_join(&[&installed, "extensions", "index.ts"]), EXT);
        f.settings.set_project_packages(&[pkg(json!("npm:example@^1.0.0"))]).unwrap();
        let result = f.pm.resolve(None).await.unwrap();
        assert!(result.extensions.iter().any(|r| is_enabled(r, "extensions/index.ts")));
        assert!(f.runner.captures().is_empty());
    }
    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "1.0.0");
    f.settings.set_project_packages(&[pkg(json!("npm:example@2.0.0"))]).unwrap();
    f.pm.resolve(None).await.unwrap();
    let root = f.path(&[".pi", "npm"]);
    assert_eq!(f.runner.runs(), vec![call("npm", &["install", "example@2.0.0", "--prefix", &root, "--legacy-peer-deps"], None)]);
}

#[tokio::test]
async fn missing_source_handler_controls_installs() {
    let _g = env_lock();
    let f = Fixture::new();
    f.settings.set_packages(&[pkg(json!("npm:missing"))]);
    let skip: MissingSourceHandler = Arc::new(|_| Box::pin(async { Ok(MissingSourceAction::Skip) }));
    assert_eq!(f.pm.resolve(Some(skip)).await.unwrap().install_failures, Some(vec![]));
    assert!(f.runner.runs().is_empty());
    let error: MissingSourceHandler = Arc::new(|_| Box::pin(async { Ok(MissingSourceAction::Error) }));
    assert_eq!(f.pm.resolve(Some(error)).await.unwrap_err().to_string(), "Missing source: npm:missing");
    let install: MissingSourceHandler = Arc::new(|_| Box::pin(async { Ok(MissingSourceAction::Install) }));
    f.pm.resolve(Some(install)).await.unwrap();
    assert_eq!(f.runner.runs().len(), 1);
}

#[tokio::test]
async fn reports_available_updates_for_unpinned_npm_packages() {
    let _g = env_lock();
    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "1.0.0");
    f.settings.set_project_packages(&[pkg(json!("npm:example"))]).unwrap();
    f.runner.on_capture(|_| Ok("\"1.2.3\"".to_string()));
    assert_eq!(
        f.pm.check_for_available_updates().await.unwrap(),
        vec![PackageUpdate {
            source: "npm:example".to_string(),
            display_name: "example".to_string(),
            r#type: PackageUpdateType::Npm,
            scope: SettingsScope::Project,
        }]
    );
    f.runner.on_capture(|_| Ok("\"0.9.0\"".to_string()));
    assert!(f.pm.check_for_available_updates().await.unwrap().is_empty());
}

#[tokio::test]
async fn skips_pinned_packages_when_checking_for_updates() {
    let _g = env_lock();
    let f = Fixture::new();
    write_installed(&f.path(&[".pi", "npm", "node_modules", "example"]), "example", "1.0.0");
    mkdir(&f.pm.get_git_install_path(&git(&f.pm, "git:github.com/example/repo@v1"), SourceScope::Project).unwrap());
    f.settings
        .set_project_packages(&[pkg(json!("npm:example@1.0.0")), pkg(json!("git:github.com/example/repo@v1"))])
        .unwrap();
    assert!(f.pm.check_for_available_updates().await.unwrap().is_empty());
    assert!(f.runner.captures().is_empty());
}

#[tokio::test]
async fn reports_git_updates_from_ls_remote() {
    let _g = env_lock();
    let f = Fixture::new();
    mkdir(&f.agent_path(&["git", "github.com", "example", "repo"]));
    f.settings.set_packages(&[pkg(json!("git:github.com/example/repo"))]);
    let remote = "a".repeat(40);
    let r = remote.clone();
    f.runner.on_capture(move |c| match c.args.join(" ").as_str() {
        "rev-parse HEAD" => Ok("b".repeat(40)),
        "rev-parse --abbrev-ref @{upstream}" => Ok("origin/main".to_string()),
        "ls-remote origin refs/heads/main" => Ok(format!("{r}\trefs/heads/main")),
        other => bail!("unexpected {other}"),
    });
    let updates = f.pm.check_for_available_updates().await.unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].display_name, "github.com/example/repo");
    assert_eq!(updates[0].r#type, PackageUpdateType::Git);
}

#[tokio::test]
async fn uses_npm_view_to_fetch_latest_version() {
    let _g = env_lock();
    let f = Fixture::new();
    f.runner.on_capture(|_| Ok("\"1.2.3\"".to_string()));
    assert_eq!(f.pm.get_latest_npm_version("example", None).await.unwrap(), "1.2.3");
    assert_eq!(f.runner.captures(), vec![call("npm", &["view", "example", "version", "--json"], Some(&f.temp))]);

    let f = Fixture::with_settings(json!({"npmCommand": ["mise", "exec", "node@20", "--", "npm"]}));
    f.runner.on_capture(|_| Ok("\"1.2.3\"".to_string()));
    assert_eq!(f.pm.get_latest_npm_version("@scope/pkg", None).await.unwrap(), "1.2.3");
    assert_eq!(
        f.runner.captures(),
        vec![call("mise", &["exec", "node@20", "--", "npm", "view", "@scope/pkg", "version", "--json"], Some(&f.temp))]
    );
    f.runner.on_capture(|_| Ok(String::new()));
    assert_eq!(f.pm.get_latest_npm_version("x", None).await.unwrap_err().to_string(), "Empty response from npm view");
    f.runner.on_capture(|_| Ok("{}".to_string()));
    assert_eq!(f.pm.get_latest_npm_version("x", None).await.unwrap_err().to_string(), "Unexpected response from npm view");
}

// ---------------------------------------------------------------------------
// issue #7187 malformed package manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ignores_invalid_manifest_resource_fields() {
    let _g = env_lock();
    let f = Fixture::with_settings(json!({"packages": ["npm:bad-package"]}));
    let package_dir = f.agent_path(&["npm", "node_modules", "bad-package"]);
    let skill_path = node_join(&[&package_dir, "skills", "bad", "SKILL.md"]);
    let prompt_path = node_join(&[&package_dir, "prompts", "valid.md"]);
    write(&skill_path, "---\nname: bad\ndescription: Must not load\n---\n");
    write(&prompt_path, "Valid prompt\n");
    write(
        &node_join(&[&package_dir, "package.json"]),
        &json!({"name": "bad-package", "version": "1.0.0", "pi": {"skills": "./skills", "prompts": ["./prompts"]}}).to_string(),
    );
    let resources = f.pm.resolve(None).await.unwrap();
    assert!(!resources.skills.iter().any(|r| r.path == skill_path));
    assert!(resources.prompts.iter().any(|r| r.path == prompt_path));
}

// ---------------------------------------------------------------------------
// built-in npm client (no npm executable)
// ---------------------------------------------------------------------------

struct NoNpmRunner(Arc<MockRunner>);

#[async_trait::async_trait]
impl PackageCommandRunner for NoNpmRunner {
    async fn run(&self, command: &str, args: &[String], cwd: Option<&str>) -> anyhow::Result<()> {
        self.0.run(command, args, cwd).await
    }
    async fn run_capture(&self, command: &str, args: &[String], options: &CaptureOptions) -> anyhow::Result<String> {
        self.0.run_capture(command, args, options).await
    }
    fn run_sync(&self, command: &str, args: &[String]) -> anyhow::Result<String> {
        self.0.run_sync(command, args)
    }
    fn command_exists(&self, command: &str) -> bool {
        command != "npm"
    }
}

#[tokio::test]
async fn installs_updates_and_removes_with_builtin_npm_client() {
    use super::npm_registry::test_support::{FakeVersion, serve_registry};
    let _g = env_lock();
    let fake = |name: &str, version: &str, deps: &[(&str, &str)]| FakeVersion {
        name: name.to_string(),
        version: version.to_string(),
        deps: deps.iter().map(|(n, s)| (n.to_string(), s.to_string())).collect(),
        files: vec![("extensions/index.ts".to_string(), EXT.to_string())],
        corrupt: false,
    };
    let registry = serve_registry(vec![
        fake("pi-tools", "1.0.0", &[("helper", "^1.0.0")]),
        fake("pi-tools", "1.1.0", &[("helper", "^1.0.0")]),
        fake("helper", "1.0.0", &[]),
    ])
    .await;
    let f = Fixture::new();
    write(&f.path(&[".npmrc"]), &format!("registry={registry}\n"));
    let mock = MockRunner::new();
    let pm = DefaultPackageManager::with_command_runner(
        &PackageManagerOptions {
            cwd: f.temp.clone(),
            agent_dir: f.agent.clone(),
            settings_manager: f.settings.clone(),
            builtin_extensions: None,
        },
        Arc::new(NoNpmRunner(mock.clone())),
    );

    pm.install_and_persist("npm:pi-tools@1.0.0", None).await.unwrap();
    let installed = f.agent_path(&["npm", "node_modules", "pi-tools"]);
    assert_eq!(pm.get_installed_npm_version(&installed).as_deref(), Some("1.0.0"));
    assert!(exists(&f.agent_path(&["npm", "node_modules", "helper", "package.json"])));
    assert!(mock.runs().is_empty());
    let resolved = pm.resolve(None).await.unwrap();
    assert!(resolved.extensions.iter().any(|r| r.path == node_join(&[&installed, "extensions", "index.ts"])));

    // Unpinned entry updates to the latest version.
    f.settings.set_packages(&[pkg(json!("npm:pi-tools"))]);
    assert_eq!(pm.check_for_available_updates().await.unwrap().len(), 1);
    pm.update(None).await.unwrap();
    assert_eq!(pm.get_installed_npm_version(&installed).as_deref(), Some("1.1.0"));
    assert!(pm.check_for_available_updates().await.unwrap().is_empty());

    assert!(pm.remove_and_persist("npm:pi-tools", None).await.unwrap());
    assert!(!exists(&installed));
    assert!(!exists(&f.agent_path(&["npm", "node_modules", "helper"])));
    assert!(mock.runs().is_empty());
}

// ---------------------------------------------------------------------------
// git-update.test.ts (real git with local remotes)
// ---------------------------------------------------------------------------

fn sh_git(args: &[&str], cwd: &str) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_repo(dir: &str) {
    mkdir(dir);
    sh_git(&["init", "--initial-branch=main"], dir);
    sh_git(&["config", "--local", "user.email", "test@test.com"], dir);
    sh_git(&["config", "--local", "user.name", "Test"], dir);
    sh_git(&["config", "--local", "commit.gpgsign", "false"], dir);
    sh_git(&["config", "--local", "tag.gpgsign", "false"], dir);
}

fn commit(dir: &str, file: &str, content: &str, message: &str) -> String {
    std::fs::write(node_join(&[dir, file]), content).unwrap();
    sh_git(&["add", file], dir);
    sh_git(&["commit", "-m", message], dir);
    sh_git(&["rev-parse", "HEAD"], dir)
}

fn git_available() -> bool {
    find_executable("git").is_some()
}

/// Runs real commands but records them and skips npm (like the TS test override).
struct RecordingRunner {
    commands: StdMutex<Vec<String>>,
}

#[async_trait::async_trait]
impl PackageCommandRunner for RecordingRunner {
    async fn run(&self, command: &str, args: &[String], cwd: Option<&str>) -> anyhow::Result<()> {
        self.commands.lock().unwrap().push(format!("{command} {}", args.join(" ")));
        if command == "npm" {
            return Ok(());
        }
        let output = std::process::Command::new(command)
            .args(args)
            .current_dir(cwd.unwrap_or("."))
            .output()?;
        if !output.status.success() {
            bail!("Command failed: {command} {}\n{}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }
    async fn run_capture(&self, command: &str, args: &[String], options: &CaptureOptions) -> anyhow::Result<String> {
        ProcessCommandRunner.run_capture(command, args, options).await
    }
    fn run_sync(&self, command: &str, args: &[String]) -> anyhow::Result<String> {
        ProcessCommandRunner.run_sync(command, args)
    }
}

struct GitFixture {
    f: Fixture,
    remote: String,
    installed: String,
    runner: Arc<RecordingRunner>,
    pm: DefaultPackageManager,
}

const GIT_SOURCE: &str = "git:github.com/test/extension";

fn git_fixture() -> GitFixture {
    let f = Fixture::new();
    let remote = f.path(&["remote"]);
    let installed = f.agent_path(&["git", "github.com", "test", "extension"]);
    let runner = Arc::new(RecordingRunner {
        commands: StdMutex::new(Vec::new()),
    });
    let pm = DefaultPackageManager::with_command_runner(
        &PackageManagerOptions {
            cwd: f.temp.clone(),
            agent_dir: f.agent.clone(),
            settings_manager: f.settings.clone(),
            builtin_extensions: None,
        },
        runner.clone(),
    );
    GitFixture { f, remote, installed, runner, pm }
}

fn clone_remote(g: &GitFixture) {
    mkdir(&g.f.agent_path(&["git", "github.com", "test"]));
    sh_git(&["clone", &g.remote, &g.installed], &g.f.temp);
    sh_git(&["config", "--local", "user.email", "test@test.com"], &g.installed);
    sh_git(&["config", "--local", "user.name", "Test"], &g.installed);
}

#[tokio::test]
async fn git_update_skips_reset_clean_and_install_when_up_to_date() {
    if !git_available() {
        return;
    }
    let g = git_fixture();
    init_repo(&g.remote);
    std::fs::write(node_join(&[&g.remote, "package.json"]), "{\"name\":\"test-extension\",\"version\":\"1.0.0\"}").unwrap();
    commit(&g.remote, "extension.ts", "// v1", "Initial commit");
    clone_remote(&g);
    g.f.settings.set_packages(&[pkg(json!(GIT_SOURCE))]);
    g.pm.update(None).await.unwrap();
    let commands = g.runner.commands.lock().unwrap().clone();
    assert!(commands.contains(&"git fetch --prune --no-tags origin +refs/heads/main:refs/remotes/origin/main".to_string()));
    assert!(!commands.iter().any(|c| c.starts_with("git reset") || c == "git clean -fdx" || c.starts_with("npm install")));
}

#[tokio::test]
async fn git_update_follows_new_commits_and_history_rewrites() {
    if !git_available() {
        return;
    }
    let g = git_fixture();
    init_repo(&g.remote);
    commit(&g.remote, "extension.ts", "// v1", "Initial commit");
    clone_remote(&g);
    g.f.settings.set_packages(&[pkg(json!(GIT_SOURCE))]);
    let new_commit = commit(&g.remote, "extension.ts", "// v2", "Second commit");
    g.pm.update(None).await.unwrap();
    assert_eq!(sh_git(&["rev-parse", "HEAD"], &g.installed), new_commit);
    assert_eq!(std::fs::read_to_string(node_join(&[&g.installed, "extension.ts"])).unwrap(), "// v2");

    commit(&g.remote, "extension.ts", "// v3", "v3");
    g.pm.update(None).await.unwrap();
    assert_eq!(std::fs::read_to_string(node_join(&[&g.installed, "extension.ts"])).unwrap(), "// v3");
    sh_git(&["reset", "--hard", "HEAD~2"], &g.remote);
    commit(&g.remote, "extension.ts", "// rewrite-a", "Rewrite A");
    let final_commit = commit(&g.remote, "extension.ts", "// rewrite-b", "Rewrite B");
    g.pm.update(None).await.unwrap();
    assert_eq!(sh_git(&["rev-parse", "HEAD"], &g.installed), final_commit);
    assert_eq!(std::fs::read_to_string(node_join(&[&g.installed, "extension.ts"])).unwrap(), "// rewrite-b");
}

#[tokio::test]
async fn git_update_checks_out_configured_pinned_ref() {
    if !git_available() {
        return;
    }
    let g = git_fixture();
    init_repo(&g.remote);
    let v1 = commit(&g.remote, "extension.ts", "// v1", "Initial commit");
    sh_git(&["tag", "v1"], &g.remote);
    let v2 = commit(&g.remote, "extension.ts", "// v2", "Second commit");
    sh_git(&["tag", "v2"], &g.remote);
    clone_remote(&g);
    sh_git(&["checkout", "v1"], &g.installed);
    assert_eq!(sh_git(&["rev-parse", "HEAD"], &g.installed), v1);
    let pinned = format!("{GIT_SOURCE}@v2");
    g.f.settings.set_packages(&[pkg(json!(pinned))]);
    g.pm.update(None).await.unwrap();
    assert_eq!(sh_git(&["rev-parse", "HEAD"], &g.installed), v2);
    sh_git(&["checkout", "v1"], &g.installed);
    g.pm.update(Some(&pinned)).await.unwrap();
    assert_eq!(sh_git(&["rev-parse", "HEAD"], &g.installed), v2);
}

#[tokio::test]
async fn refreshes_cached_temporary_git_sources_when_resolving() {
    let _g = env_lock();
    let f = Fixture::new();
    let cached = f.pm.get_git_install_path(&git(&f.pm, GIT_SOURCE), SourceScope::Temporary).unwrap();
    let extension_file = node_join(&[&cached, "pi-extensions", "session-breakdown.ts"]);
    write(&node_join(&[&cached, "package.json"]), &json!({"pi": {"extensions": ["./pi-extensions"]}}).to_string());
    write(&extension_file, "// stale");
    let ef = extension_file.clone();
    f.runner.on_run(move |c| {
        if c.command == "git" && c.args[0] == "reset" {
            std::fs::write(&ef, "// fresh").unwrap();
        }
        Ok(())
    });
    f.runner.on_capture(|c| match (c.args[0].as_str(), c.args[1].as_str()) {
        ("rev-parse", "HEAD") => Ok("local-head".to_string()),
        ("rev-parse", "@{upstream}") => Ok("remote-head".to_string()),
        ("rev-parse", "--abbrev-ref") => Ok("origin/main".to_string()),
        _ => Ok(String::new()),
    });
    f.pm
        .resolve_extension_sources(&s(&[GIT_SOURCE]), Some(&PackageManagerResolveExtensionSourcesOptions { local: None, temporary: Some(true) }))
        .await
        .unwrap();
    assert!(f.runner.ran(
        "git",
        &["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"],
        Some(&cached)
    ));
    assert_eq!(std::fs::read_to_string(&extension_file).unwrap(), "// fresh");
}

#[tokio::test]
async fn package_install_failure_does_not_abort_resolution() {
    let _guard = env_lock();
    let f =
        Fixture::with_settings(json!({"npmCommand": ["npm"], "packages": ["npm:broken-package"]}));
    f.runner.on_run(|_| bail!("simulated install failure"));
    write(&f.agent_path(&["prompts", "healthy.md"]), "healthy prompt");
    let result =
        f.pm.resolve(None)
            .await
            .expect("startup must survive an install failure");
    assert_eq!(result.prompts.len(), 1);
    assert!(result.extensions.is_empty());
    let failure = PackageInstallFailure {
        source: "npm:broken-package".to_string(),
        scope: SourceScope::User,
        message: "simulated install failure".to_string(),
    };
    assert_eq!(result.install_failures, Some(vec![failure.clone()]));
    assert_eq!(
        serde_json::to_value(&result).unwrap()["installFailures"],
        json!([
            {"source": "npm:broken-package", "scope": "user", "message": "simulated install failure"}
        ])
    );
    // The setting survives and resolution retries on the next startup/reload.
    assert_eq!(
        f.pm.resolve(None).await.unwrap().install_failures,
        Some(vec![failure])
    );
    assert_eq!(f.runner.runs().len(), 2);
    // Explicit mutation commands must still fail.
    assert!(f.pm.install("npm:broken-package", None).await.is_err());
    assert!(f.pm.update(None).await.is_err());
}

#[tokio::test]
async fn package_install_failure_reports_project_git_and_temporary_sources() {
    let _guard = env_lock();
    let f = Fixture::with_settings(json!({"npmCommand": ["npm"]}));
    f.settings
        .set_project_packages(&[pkg(json!("git:github.com/example/broken"))])
        .unwrap();
    f.runner.on_run(|_| bail!("simulated git failure"));
    let result = f.pm.resolve(None).await.unwrap();
    assert_eq!(
        result.install_failures.unwrap(),
        vec![PackageInstallFailure {
            source: "git:github.com/example/broken".to_string(),
            scope: SourceScope::Project,
            message: "simulated git failure".to_string(),
        }]
    );
    let result =
        f.pm.resolve_extension_sources(
            &s(&["npm:broken-package"]),
            Some(&PackageManagerResolveExtensionSourcesOptions {
                temporary: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        result.install_failures.unwrap()[0].scope,
        SourceScope::Temporary
    );
}

#[tokio::test]
async fn package_install_failure_preserves_missing_source_callback_errors() {
    let _guard = env_lock();
    let f =
        Fixture::with_settings(json!({"npmCommand": ["npm"], "packages": ["npm:broken-package"]}));
    let handler: MissingSourceHandler =
        Arc::new(|_| Box::pin(async { bail!("caller policy failed") }));
    assert_eq!(
        f.pm.resolve(Some(handler)).await.unwrap_err().to_string(),
        "caller policy failed"
    );
    assert!(f.runner.runs().is_empty());
    f.runner.on_run(|_| bail!("install failed"));
    let handler: MissingSourceHandler =
        Arc::new(|_| Box::pin(async { Ok(MissingSourceAction::Install) }));
    assert_eq!(
        f.pm.resolve(Some(handler))
            .await
            .unwrap()
            .install_failures
            .unwrap()[0]
            .message,
        "install failed"
    );
}
