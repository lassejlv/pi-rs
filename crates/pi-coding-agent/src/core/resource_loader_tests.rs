//! Port of `test/resource-loader.test.ts` (context files, skills, prompts, system prompts,
//! overrides, extendResources). Extension and theme cases live with the extension loader and
//! theme modules.

use super::*;
use crate::core::source_info::{CreateSyntheticSourceInfoOptions, create_synthetic_source_info};
use std::path::PathBuf;

struct Dirs {
    _temp: tempfile::TempDir,
    temp_dir: String,
    agent_dir: String,
    cwd: String,
}

impl Dirs {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let temp_dir = temp.path().to_string_lossy().into_owned();
        let agent_dir = node_join(&[&temp_dir, "agent"]);
        let cwd = node_join(&[&temp_dir, "project"]);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Self {
            _temp: temp,
            temp_dir,
            agent_dir,
            cwd,
        }
    }

    fn options(&self) -> DefaultResourceLoaderOptions {
        DefaultResourceLoaderOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            ..Default::default()
        }
    }

    fn loader(&self) -> DefaultResourceLoader {
        DefaultResourceLoader::new(&self.options())
    }
}

fn write(path: &str, content: &str) {
    let parent = node_dirname(path);
    std::fs::create_dir_all(parent).unwrap();
    std::fs::write(path, content).unwrap();
}

fn mkdir(path: &str) {
    std::fs::create_dir_all(path).unwrap();
}

fn context(path: &str, content: &str) -> ContextFile {
    ContextFile {
        path: path.to_string(),
        content: content.to_string(),
    }
}

fn contents(files: &[ContextFile]) -> Vec<&str> {
    files.iter().map(|f| f.content.as_str()).collect()
}

fn load_context(cwd: &str, agent_dir: &str) -> Vec<ContextFile> {
    load_project_context_files(&LoadProjectContextFilesOptions {
        cwd: cwd.to_string(),
        agent_dir: agent_dir.to_string(),
    })
}

// ---------------------------------------------------------------------------
// loadProjectContextFiles
// ---------------------------------------------------------------------------

#[test]
fn prefers_agents_override_while_preserving_ancestor_layering() {
    let dirs = Dirs::new();
    let nested = node_join(&[&dirs.cwd, "service"]);
    mkdir(&nested);
    write(&node_join(&[&dirs.agent_dir, "AGENTS.md"]), "global instructions");
    write(&node_join(&[&dirs.agent_dir, "AGENTS.override.md"]), "global override");
    write(&node_join(&[&dirs.cwd, "AGENTS.md"]), "project instructions");
    write(&node_join(&[&nested, "AGENTS.md"]), "service instructions");
    write(&node_join(&[&nested, "AGENTS.override.md"]), "service override");

    assert_eq!(
        load_context(&nested, &dirs.agent_dir),
        vec![
            context(
                &node_join(&[&dirs.agent_dir, "AGENTS.override.md"]),
                "global override"
            ),
            context(&node_join(&[&dirs.cwd, "AGENTS.md"]), "project instructions"),
            context(&node_join(&[&nested, "AGENTS.override.md"]), "service override"),
        ]
    );
}

#[test]
fn ignores_context_file_candidates_that_are_directories() {
    let dirs = Dirs::new();
    mkdir(&node_join(&[&dirs.cwd, "AGENTS.override.md"]));
    mkdir(&node_join(&[&dirs.cwd, "AGENTS.md"]));
    write(&node_join(&[&dirs.cwd, "CLAUDE.md"]), "Fallback instructions");
    let files = load_context(&dirs.cwd, &dirs.agent_dir);
    assert!(files.contains(&context(
        &node_join(&[&dirs.cwd, "CLAUDE.md"]),
        "Fallback instructions"
    )));
}

/// Main repo at `<temp>/outer/main` with a linked worktree at `main/worktrees/feat`.
struct NestedWorktree {
    outer: String,
    main: String,
    worktree: String,
    worktree_src: String,
}

fn link_worktree(main_dir: &str, worktree_dir: &str, name: &str) {
    let git_dir = node_join(&[main_dir, ".git", "worktrees", name]);
    mkdir(&git_dir);
    write(&node_join(&[main_dir, ".git", "HEAD"]), "ref: refs/heads/main\n");
    write(&node_join(&[&git_dir, "HEAD"]), "ref: refs/heads/feat\n");
    write(&node_join(&[&git_dir, "commondir"]), "../..");
    write(
        &node_join(&[worktree_dir, ".git"]),
        &format!("gitdir: {git_dir}\n"),
    );
}

fn setup_nested_worktree(dirs: &Dirs) -> NestedWorktree {
    let outer = node_join(&[&dirs.temp_dir, "outer"]);
    let main = node_join(&[&outer, "main"]);
    let worktree = node_join(&[&main, "worktrees", "feat"]);
    let worktree_src = node_join(&[&worktree, "src"]);
    mkdir(&worktree_src);
    link_worktree(&main, &worktree, "feat");
    NestedWorktree {
        outer,
        main,
        worktree,
        worktree_src,
    }
}

#[cfg(unix)]
fn symlink(target: &str, link: &str) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn skips_main_repo_duplicate_when_worktree_root_has_context() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    write(&node_join(&[&w.main, "AGENTS.md"]), "main repo instructions");
    write(&node_join(&[&w.worktree, "AGENTS.md"]), "worktree instructions");
    assert_eq!(
        contents(&load_context(&w.worktree_src, &dirs.agent_dir)),
        vec!["worktree instructions"]
    );
}

#[cfg(unix)]
#[test]
fn loads_worktree_context_once_when_it_symlinks_to_main_repo_file() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    write(&node_join(&[&w.main, "AGENTS.md"]), "main repo instructions");
    symlink(
        &node_join(&[&w.main, "AGENTS.md"]),
        &node_join(&[&w.worktree, "AGENTS.md"]),
    );
    let files = load_context(&w.worktree_src, &dirs.agent_dir);
    assert_eq!(
        files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        vec![node_join(&[&w.worktree, "AGENTS.md"])]
    );
}

#[cfg(unix)]
#[test]
fn skips_main_repo_duplicate_when_main_file_is_symlink() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    let shared = node_join(&[&w.outer, "shared-agents.md"]);
    write(&shared, "shared instructions");
    symlink(&shared, &node_join(&[&w.main, "AGENTS.md"]));
    symlink(&shared, &node_join(&[&w.worktree, "AGENTS.md"]));
    let files = load_context(&w.worktree_src, &dirs.agent_dir);
    assert_eq!(
        files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        vec![node_join(&[&w.worktree, "AGENTS.md"])]
    );
}

#[test]
fn inherits_main_repo_context_when_worktree_root_has_none() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    write(&node_join(&[&w.main, "AGENTS.md"]), "main repo instructions");
    assert_eq!(
        contents(&load_context(&w.worktree_src, &dirs.agent_dir)),
        vec!["main repo instructions"]
    );
}

#[test]
fn only_skips_the_same_filename() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    write(&node_join(&[&w.main, "CLAUDE.md"]), "main repo instructions");
    write(&node_join(&[&w.worktree, "AGENTS.md"]), "worktree instructions");
    assert_eq!(
        contents(&load_context(&w.worktree_src, &dirs.agent_dir)),
        vec!["main repo instructions", "worktree instructions"]
    );
}

#[test]
fn does_not_skip_container_context_in_bare_layout() {
    let dirs = Dirs::new();
    let proj = node_join(&[&dirs.temp_dir, "proj"]);
    let bare = node_join(&[&proj, ".bare"]);
    let worktree = node_join(&[&proj, "main"]);
    let worktree_git_dir = node_join(&[&bare, "worktrees", "main"]);
    mkdir(&worktree_git_dir);
    mkdir(&worktree);
    write(&node_join(&[&bare, "HEAD"]), "ref: refs/heads/main\n");
    write(&node_join(&[&worktree_git_dir, "HEAD"]), "ref: refs/heads/main\n");
    write(&node_join(&[&worktree_git_dir, "commondir"]), "../..");
    write(
        &node_join(&[&worktree, ".git"]),
        &format!("gitdir: {worktree_git_dir}\n"),
    );
    write(&node_join(&[&proj, "AGENTS.md"]), "container instructions");
    write(&node_join(&[&worktree, "AGENTS.md"]), "worktree instructions");
    assert_eq!(
        contents(&load_context(&worktree, &dirs.agent_dir)),
        vec!["container instructions", "worktree instructions"]
    );
}

#[test]
fn keeps_loading_ancestors_above_main_repo() {
    let dirs = Dirs::new();
    let w = setup_nested_worktree(&dirs);
    write(&node_join(&[&w.outer, "AGENTS.md"]), "outer instructions");
    write(&node_join(&[&w.main, "AGENTS.md"]), "main repo instructions");
    write(&node_join(&[&w.worktree, "AGENTS.md"]), "worktree instructions");
    assert_eq!(
        contents(&load_context(&w.worktree_src, &dirs.agent_dir)),
        vec!["outer instructions", "worktree instructions"]
    );
}

#[test]
fn does_not_skip_for_sibling_worktree() {
    let dirs = Dirs::new();
    let outer = node_join(&[&dirs.temp_dir, "outer"]);
    let main = node_join(&[&outer, "main"]);
    let sib = node_join(&[&outer, "sib-feat"]);
    let sib_src = node_join(&[&sib, "src"]);
    mkdir(&sib_src);
    mkdir(&main);
    write(&node_join(&[&outer, "AGENTS.md"]), "outer instructions");
    write(&node_join(&[&sib, "AGENTS.md"]), "sibling worktree instructions");
    link_worktree(&main, &sib, "sib");
    assert_eq!(
        contents(&load_context(&sib_src, &dirs.agent_dir)),
        vec!["outer instructions", "sibling worktree instructions"]
    );
}

#[test]
fn does_not_skip_superproject_context_from_submodule() {
    let dirs = Dirs::new();
    let sup = node_join(&[&dirs.temp_dir, "super"]);
    let sub = node_join(&[&sup, "vendor", "lib"]);
    let sub_src = node_join(&[&sub, "src"]);
    mkdir(&sub_src);
    write(&node_join(&[&sup, "AGENTS.md"]), "superproject instructions");
    write(&node_join(&[&sub, "AGENTS.md"]), "submodule instructions");
    let sub_git_dir = node_join(&[&sup, ".git", "modules", "vendor", "lib"]);
    mkdir(&sub_git_dir);
    write(&node_join(&[&sub_git_dir, "HEAD"]), "ref: refs/heads/main\n");
    write(&node_join(&[&sub, ".git"]), &format!("gitdir: {sub_git_dir}\n"));
    assert_eq!(
        contents(&load_context(&sub_src, &dirs.agent_dir)),
        vec!["superproject instructions", "submodule instructions"]
    );
}

#[test]
fn keeps_climbing_past_ordinary_repo_root() {
    let dirs = Dirs::new();
    let outer = node_join(&[&dirs.temp_dir, "outer"]);
    let repo = node_join(&[&outer, "repo"]);
    let leaf = node_join(&[&repo, "src"]);
    mkdir(&leaf);
    write(&node_join(&[&repo, ".git", "HEAD"]), "ref: refs/heads/main\n");
    write(&node_join(&[&outer, "AGENTS.md"]), "outer instructions");
    write(&node_join(&[&repo, "AGENTS.md"]), "repo instructions");
    write(&node_join(&[&leaf, "AGENTS.md"]), "leaf instructions");
    assert_eq!(
        contents(&load_context(&leaf, &dirs.agent_dir)),
        vec!["outer instructions", "repo instructions", "leaf instructions"]
    );
}

#[test]
fn climbs_normally_when_gitdir_target_is_missing() {
    let dirs = Dirs::new();
    let repo = node_join(&[&dirs.temp_dir, "corrupt"]);
    let src = node_join(&[&repo, "src"]);
    mkdir(&src);
    write(
        &node_join(&[&repo, ".git"]),
        "gitdir: /nonexistent/path/worktrees/feat\n",
    );
    write(&node_join(&[&repo, "AGENTS.md"]), "repo instructions");
    write(&node_join(&[&src, "AGENTS.md"]), "src instructions");
    assert_eq!(
        contents(&load_context(&src, &dirs.agent_dir)),
        vec!["repo instructions", "src instructions"]
    );
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn builtin_extension_detection() {
    let factory: crate::core::extensions::types::ExtensionFactory =
        Arc::new(|_| Box::pin(async { Ok(()) }));
    let named = |builtin: Option<bool>| {
        InlineExtension::Named(NamedInlineExtension {
            name: "mcp".to_string(),
            factory: factory.clone(),
            hidden: None,
            replaceable: None,
            builtin,
        })
    };
    assert!(is_builtin_extension(&named(Some(true))));
    assert!(!is_builtin_extension(&named(None)));
    assert!(!is_builtin_extension(&InlineExtension::Factory(
        factory.clone()
    )));
}

#[test]
fn collects_host_dependency_warnings() {
    let dirs = Dirs::new();
    let package_root = node_join(&[&dirs.temp_dir, "extension-package"]);
    write(
        &node_join(&[&package_root, "package.json"]),
        r#"{"dependencies": {"typebox": "1", "@earendil-works/pi-coding-agent": "1.0.0", "left-pad": "1"}}"#,
    );
    let extension_path = node_join(&[&package_root, "extensions", "ext.ts"]);
    let mut metadata = IndexMap::new();
    metadata.insert(
        extension_path.clone(),
        PathMetadata {
            source: "local".to_string(),
            scope: SourceScope::User,
            origin: SourceOrigin::Package,
            base_dir: None,
            package_root: Some(package_root.clone()),
        },
    );
    let warnings = collect_extension_package_warnings(std::slice::from_ref(&extension_path), &metadata).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].path, node_join(&[&package_root, "package.json"]));
    assert_eq!(
        warnings[0].warning,
        "Host-provided extension packages must be declared in peerDependencies with a \"*\" range, not dependencies: @earendil-works/pi-coding-agent, typebox. Installed copies can bypass the extension loader and create duplicate runtime modules."
    );

    write(&node_join(&[&package_root, "package.json"]), "{");
    assert!(collect_extension_package_warnings(&[extension_path], &metadata).is_err());
}

#[test]
fn dedupes_prompts_by_name() {
    let prompt = |path: &str| PromptTemplate {
        name: "commit".to_string(),
        description: String::new(),
        argument_hint: None,
        content: String::new(),
        source_info: create_synthetic_source_info(
            path,
            &CreateSyntheticSourceInfoOptions {
                source: "local".to_string(),
                scope: None,
                origin: None,
                base_dir: None,
            },
        ),
        file_path: path.to_string(),
    };
    let (prompts, diagnostics) = dedupe_prompts(vec![prompt("/a/commit.md"), prompt("/b/commit.md")]);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].file_path, "/a/commit.md");
    assert_eq!(diagnostics[0].message, "name \"/commit\" collision");
    let collision = diagnostics[0].collision.as_ref().unwrap();
    assert_eq!(collision.winner_path, "/a/commit.md");
    assert_eq!(collision.loser_path, "/b/commit.md");
}

// ---------------------------------------------------------------------------
// DefaultResourceLoader.reload (needs the package manager)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovers_skills_and_prompts_from_agent_dir() {
    let dirs = Dirs::new();
    write(
        &node_join(&[&dirs.agent_dir, "skills", "test-skill.md"]),
        "---\nname: test-skill\ndescription: A test skill\n---\nSkill content here.",
    );
    let skill_dir = node_join(&[&dirs.agent_dir, "skills", "pi-skills", "browser-tools"]);
    write(
        &node_join(&[&skill_dir, "SKILL.md"]),
        "---\nname: browser-tools\ndescription: Browser tools\n---\nSkill content here.",
    );
    write(&node_join(&[&skill_dir, "EFFICIENCY.md"]), "No frontmatter here");
    write(
        &node_join(&[&dirs.agent_dir, "prompts", "test-prompt.md"]),
        "---\ndescription: A test prompt\n---\nPrompt content.",
    );

    let loader = dirs.loader();
    loader.reload(None).await.unwrap();

    let skills = loader.get_skills();
    assert!(skills.skills.iter().any(|s| s.name == "test-skill"));
    assert!(skills.skills.iter().any(|s| s.name == "browser-tools"));
    assert!(
        !skills
            .diagnostics
            .iter()
            .any(|d| d.path.as_deref().is_some_and(|p| p.ends_with("EFFICIENCY.md")))
    );
    assert!(
        loader
            .get_prompts()
            .prompts
            .iter()
            .any(|p| p.name == "test-prompt")
    );
}

#[tokio::test]
async fn reports_invalid_prompt_frontmatter_while_loading_siblings() {
    let dirs = Dirs::new();
    let invalid = node_join(&[&dirs.agent_dir, "prompts", "invalid.md"]);
    write(
        &invalid,
        "---\ndescription: Broken: unquoted colon\n---\nDo something.\n",
    );
    write(
        &node_join(&[&dirs.agent_dir, "prompts", "valid.md"]),
        "Valid prompt content.",
    );
    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    let prompts = loader.get_prompts();
    assert_eq!(
        prompts.prompts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["valid"]
    );
    assert_eq!(prompts.diagnostics.len(), 1);
    assert_eq!(prompts.diagnostics[0].path.as_deref(), Some(invalid.as_str()));
    assert!(prompts.diagnostics[0].message.contains("line 1"));
}

#[tokio::test]
async fn prefers_project_resources_over_user_on_name_collisions() {
    let dirs = Dirs::new();
    let user_prompt = node_join(&[&dirs.agent_dir, "prompts", "commit.md"]);
    let project_prompt = node_join(&[&dirs.cwd, ".pi", "prompts", "commit.md"]);
    write(&user_prompt, "User prompt");
    write(&project_prompt, "Project prompt");
    let user_skill = node_join(&[&dirs.agent_dir, "skills", "collision-skill", "SKILL.md"]);
    let project_skill = node_join(&[&dirs.cwd, ".pi", "skills", "collision-skill", "SKILL.md"]);
    write(
        &user_skill,
        "---\nname: collision-skill\ndescription: user\n---\nUser skill",
    );
    write(
        &project_skill,
        "---\nname: collision-skill\ndescription: project\n---\nProject skill",
    );

    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    let prompt = loader
        .get_prompts()
        .prompts
        .into_iter()
        .find(|p| p.name == "commit")
        .unwrap();
    assert_eq!(prompt.file_path, project_prompt);
    let skill = loader
        .get_skills()
        .skills
        .into_iter()
        .find(|s| s.name == "collision-skill")
        .unwrap();
    assert_eq!(skill.file_path, project_skill);
}

#[tokio::test]
async fn honors_overrides_for_auto_discovered_resources() {
    let dirs = Dirs::new();
    let settings_manager = SettingsManager::in_memory(None, None);
    settings_manager.set_skill_paths(&["-skills/skip-skill".to_string()]);
    settings_manager.set_prompt_template_paths(&["-prompts/skip.md".to_string()]);
    write(
        &node_join(&[&dirs.agent_dir, "skills", "skip-skill", "SKILL.md"]),
        "---\nname: skip-skill\ndescription: Skip me\n---\nContent",
    );
    write(&node_join(&[&dirs.agent_dir, "prompts", "skip.md"]), "Skip prompt");

    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        settings_manager: Some(settings_manager),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert!(!loader.get_skills().skills.iter().any(|s| s.name == "skip-skill"));
    assert!(!loader.get_prompts().prompts.iter().any(|p| p.name == "skip"));
}

#[tokio::test]
async fn discovers_and_skips_context_files() {
    let dirs = Dirs::new();
    write(
        &node_join(&[&dirs.cwd, "AGENTS.md"]),
        "# Project Guidelines\n\nBe helpful.",
    );
    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    assert!(
        loader
            .get_agents_files()
            .agents_files
            .iter()
            .any(|f| f.path.contains("AGENTS.md"))
    );

    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        no_context_files: Some(true),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert!(loader.get_agents_files().agents_files.is_empty());
}

#[tokio::test]
async fn discovers_system_prompt_files_and_sources() {
    let dirs = Dirs::new();
    let project_system = node_join(&[&dirs.cwd, ".pi", "SYSTEM.md"]);
    write(&project_system, "Project system prompt.");
    let append = node_join(&[&dirs.cwd, ".pi", "APPEND_SYSTEM.md"]);
    write(&append, "Project append prompt.");
    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Project system prompt.")
    );
    assert_eq!(
        loader.get_system_prompt_source().map(|s| s.path),
        Some(project_system.clone())
    );
    assert_eq!(loader.get_append_system_prompt(), vec!["Project append prompt."]);
    assert_eq!(
        loader
            .get_append_system_prompt_sources()
            .into_iter()
            .map(|s| s.path)
            .collect::<Vec<_>>(),
        vec![append]
    );

    let dirs = Dirs::new();
    let global_system = node_join(&[&dirs.agent_dir, "SYSTEM.md"]);
    write(&global_system, "Global system prompt.");
    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Global system prompt.")
    );
    assert_eq!(
        loader.get_system_prompt_source().map(|s| s.path),
        Some(global_system)
    );
}

#[tokio::test]
async fn literal_and_file_backed_system_prompt_options() {
    let dirs = Dirs::new();
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        system_prompt: Some("Literal system prompt.".to_string()),
        append_system_prompt: Some(vec!["Literal append prompt.".to_string()]),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Literal system prompt.")
    );
    assert!(loader.get_system_prompt_source().is_none());
    assert_eq!(loader.get_append_system_prompt(), vec!["Literal append prompt."]);
    assert!(loader.get_append_system_prompt_sources().is_empty());

    let system_path = node_join(&[&dirs.temp_dir, "custom-system.md"]);
    write(&system_path, "Custom system prompt.");
    let append_path = node_join(&[&dirs.temp_dir, "custom-append.md"]);
    write(&append_path, "Custom append prompt.");
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        system_prompt: Some(system_path.clone()),
        append_system_prompt: Some(vec![append_path.clone(), "Literal append prompt.".to_string()]),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Custom system prompt.")
    );
    assert_eq!(
        loader.get_system_prompt_source().map(|s| s.path),
        Some(system_path)
    );
    assert_eq!(
        loader.get_append_system_prompt(),
        vec!["Custom append prompt.", "Literal append prompt."]
    );
    assert_eq!(
        loader
            .get_append_system_prompt_sources()
            .into_iter()
            .map(|s| s.path)
            .collect::<Vec<_>>(),
        vec![append_path]
    );
}

#[tokio::test]
async fn skips_project_resources_when_project_is_not_trusted() {
    let dirs = Dirs::new();
    let pi_dir = node_join(&[&dirs.cwd, ".pi"]);
    write(&node_join(&[&pi_dir, "SYSTEM.md"]), "Project system prompt.");
    write(&node_join(&[&dirs.agent_dir, "SYSTEM.md"]), "Global system prompt.");
    write(&node_join(&[&dirs.agent_dir, "AGENTS.md"]), "Global instructions");
    write(&node_join(&[&dirs.cwd, "AGENTS.md"]), "Project instructions");
    write(
        &node_join(&[&pi_dir, "skills", "project-skill", "SKILL.md"]),
        "---\nname: project-skill\ndescription: Project skill\n---\nProject skill content",
    );
    write(&node_join(&[&pi_dir, "prompts", "project.md"]), "Project prompt");
    let settings_manager = SettingsManager::create(
        &dirs.cwd,
        Some(&dirs.agent_dir),
        Some(&crate::core::settings_manager::SettingsManagerCreateOptions {
            project_trusted: Some(false),
        }),
    );
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        settings_manager: Some(settings_manager),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Global system prompt.")
    );
    let agents_files = loader.get_agents_files().agents_files;
    assert!(
        agents_files
            .iter()
            .any(|f| f.path == node_join(&[&dirs.agent_dir, "AGENTS.md"]))
    );
    assert!(
        agents_files
            .iter()
            .any(|f| f.path == node_join(&[&dirs.cwd, "AGENTS.md"]))
    );
    assert!(loader.get_extensions().extensions.is_empty());
    assert!(
        !loader
            .get_skills()
            .skills
            .iter()
            .any(|s| s.name == "project-skill")
    );
    assert!(!loader.get_prompts().prompts.iter().any(|p| p.name == "project"));
}

#[tokio::test]
async fn extend_resources_applies_extension_metadata() {
    let dirs = Dirs::new();
    let extra_skill_dir = node_join(&[&dirs.temp_dir, "extra skills", "extra-skill"]);
    let skill_path = node_join(&[&extra_skill_dir, "SKILL.md"]);
    write(
        &skill_path,
        "---\nname: extra-skill\ndescription: Extra skill\n---\nExtra content",
    );
    let extra_prompt_dir = node_join(&[&dirs.temp_dir, "extra-prompts"]);
    let prompt_path = node_join(&[&extra_prompt_dir, "extra.md"]);
    write(&prompt_path, "---\ndescription: Extra prompt\n---\nExtra prompt content");

    let loader = dirs.loader();
    loader.reload(None).await.unwrap();

    let metadata = |base_dir: &str| PathMetadata {
        source: "extension:extra".to_string(),
        scope: SourceScope::Temporary,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(base_dir.to_string()),
        package_root: None,
    };
    let skill_url = url::Url::from_directory_path(PathBuf::from(&extra_skill_dir))
        .unwrap()
        .to_string();
    loader.extend_resources(&ResourceExtensionPaths {
        skill_paths: Some(vec![ResourceExtensionPathsSkillPaths {
            path: skill_url.trim_end_matches('/').to_string(),
            metadata: metadata(&extra_skill_dir),
        }]),
        prompt_paths: Some(vec![ResourceExtensionPathsSkillPaths {
            path: prompt_path.clone(),
            metadata: metadata(&extra_prompt_dir),
        }]),
        theme_paths: None,
    });

    let skills = loader.get_skills();
    assert!(skills.diagnostics.is_empty());
    let skill = skills
        .skills
        .iter()
        .find(|s| s.name == "extra-skill")
        .unwrap();
    assert_eq!(skill.file_path, skill_path);
    assert_eq!(skill.source_info.source, "extension:extra");
    assert_eq!(skill.source_info.path, skill_path);

    let prompt = loader
        .get_prompts()
        .prompts
        .into_iter()
        .find(|p| p.name == "extra")
        .unwrap();
    assert_eq!(prompt.source_info.source, "extension:extra");
    assert_eq!(prompt.source_info.path, prompt_path);
}

#[tokio::test]
async fn no_skills_option_and_additional_paths() {
    let dirs = Dirs::new();
    write(
        &node_join(&[&dirs.agent_dir, "skills", "test-skill.md"]),
        "---\nname: test-skill\ndescription: A test skill\n---\nContent",
    );
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        no_skills: Some(true),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    assert!(loader.get_skills().skills.is_empty());

    let custom = node_join(&[&dirs.temp_dir, "custom-skills"]);
    write(
        &node_join(&[&custom, "custom.md"]),
        "---\nname: custom\ndescription: Custom skill\n---\nContent",
    );
    let missing = node_join(&[&dirs.temp_dir, "missing-skills"]);
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        no_skills: Some(true),
        additional_skill_paths: Some(vec![custom, missing.clone()]),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    let skills = loader.get_skills();
    assert!(skills.skills.iter().any(|s| s.name == "custom"));
    assert!(
        skills
            .diagnostics
            .iter()
            .any(|d| d.path.as_deref() == Some(missing.as_str()))
    );
}

#[tokio::test]
async fn applies_skills_and_system_prompt_overrides() {
    let dirs = Dirs::new();
    let injected = Skill {
        name: "injected".to_string(),
        description: "Injected skill".to_string(),
        file_path: "/fake/path".to_string(),
        base_dir: "/fake".to_string(),
        source_info: create_synthetic_source_info(
            "/fake/path",
            &CreateSyntheticSourceInfoOptions {
                source: "custom".to_string(),
                scope: None,
                origin: None,
                base_dir: None,
            },
        ),
        disable_model_invocation: false,
    };
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        skills_override: Some(Arc::new(move |_| ResourceLoaderGetSkillsResult {
            skills: vec![injected.clone()],
            diagnostics: vec![],
        })),
        system_prompt_override: Some(Arc::new(|_| Some("Custom system prompt".to_string()))),
        ..dirs.options()
    });
    loader.reload(None).await.unwrap();
    let skills = loader.get_skills().skills;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "injected");
    assert_eq!(
        loader.get_system_prompt().as_deref(),
        Some("Custom system prompt")
    );
}

// ---------------------------------------------------------------------------
// suite/regressions/2781-skill-collision-precedence.test.ts: user and project skills override
// package skills of the same name.
// ---------------------------------------------------------------------------

fn create_package_with_skill(dirs: &Dirs, name: &str, description: &str) -> String {
    let pkg_dir = node_join(&[&dirs.temp_dir, &format!("fake-package-{name}")]);
    write(
        &node_join(&[&pkg_dir, "package.json"]),
        &serde_json::to_string_pretty(&serde_json::json!({
            "name": format!("fake-pkg-{name}"), "version": "1.0.0", "pi": { "skills": [format!("skills/{name}")] }
        }))
        .unwrap(),
    );
    write(
        &node_join(&[&pkg_dir, "skills", name, "SKILL.md"]),
        &format!("---\nname: {name}\ndescription: {description}\n---\nPackage skill content"),
    );
    pkg_dir
}

fn create_skill(root: &str, name: &str, description: &str, label: &str) -> String {
    let path = node_join(&[root, "skills", name, "SKILL.md"]);
    write(&path, &format!("---\nname: {name}\ndescription: {description}\n---\n{label} skill content"));
    path
}

fn create_user_settings_with_package(dirs: &Dirs, pkg_dir: &str) {
    write(
        &node_join(&[&dirs.agent_dir, "settings.json"]),
        &serde_json::to_string_pretty(&serde_json::json!({ "packages": [pkg_dir] })).unwrap(),
    );
}

async fn web_fetch_skill(dirs: &Dirs) -> (Skill, Vec<ResourceDiagnostic>) {
    let loader = dirs.loader();
    loader.reload(None).await.unwrap();
    let result = loader.get_skills();
    let skill = result.skills.into_iter().find(|s| s.name == "web-fetch").expect("web-fetch skill");
    (skill, result.diagnostics)
}

#[tokio::test]
async fn user_auto_discovered_skill_overrides_package_skill_with_same_name() {
    let dirs = Dirs::new();
    let pkg_dir = create_package_with_skill(&dirs, "web-fetch", "Package web-fetch skill");
    let user_skill = create_skill(&dirs.agent_dir, "web-fetch", "User web-fetch override", "User");
    create_user_settings_with_package(&dirs, &pkg_dir);
    let (skill, _) = web_fetch_skill(&dirs).await;
    assert_eq!(skill.file_path, user_skill);
    assert_eq!(skill.description, "User web-fetch override");
}

#[tokio::test]
async fn project_auto_discovered_skill_overrides_package_skill_with_same_name() {
    let dirs = Dirs::new();
    let pkg_dir = create_package_with_skill(&dirs, "web-fetch", "Package web-fetch skill");
    let project_skill = create_skill(&node_join(&[&dirs.cwd, ".pi"]), "web-fetch", "Project web-fetch override", "Project");
    create_user_settings_with_package(&dirs, &pkg_dir);
    let (skill, _) = web_fetch_skill(&dirs).await;
    assert_eq!(skill.file_path, project_skill);
    assert_eq!(skill.description, "Project web-fetch override");
}

#[tokio::test]
async fn project_skill_overrides_user_skill_which_overrides_package_skill() {
    let dirs = Dirs::new();
    let pkg_dir = create_package_with_skill(&dirs, "web-fetch", "Package web-fetch skill");
    create_skill(&dirs.agent_dir, "web-fetch", "User web-fetch override", "User");
    let project_skill = create_skill(&node_join(&[&dirs.cwd, ".pi"]), "web-fetch", "Project web-fetch override", "Project");
    create_user_settings_with_package(&dirs, &pkg_dir);
    let (skill, _) = web_fetch_skill(&dirs).await;
    assert_eq!(skill.file_path, project_skill);
    assert_eq!(skill.description, "Project web-fetch override");
}

#[tokio::test]
async fn collision_diagnostics_report_the_package_skill_as_loser_when_the_user_skill_wins() {
    let dirs = Dirs::new();
    let pkg_dir = create_package_with_skill(&dirs, "web-fetch", "Package web-fetch skill");
    create_skill(&dirs.agent_dir, "web-fetch", "User web-fetch override", "User");
    create_user_settings_with_package(&dirs, &pkg_dir);
    let (_, diagnostics) = web_fetch_skill(&dirs).await;
    let collision = diagnostics
        .iter()
        .map(|d| serde_json::to_value(d).unwrap())
        .find(|d| d["type"] == "collision" && d["collision"]["name"] == "web-fetch")
        .expect("collision diagnostic");
    assert!(collision["collision"]["loserPath"].as_str().unwrap().contains("fake-package"), "{collision}");
}

#[tokio::test]
async fn package_install_failure_warns_for_each_scope_and_keeps_loading() {
    // Serialize with package-manager tests that toggle PI_OFFLINE.
    let _guard = crate::core::package_manager::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for scope in [
        SourceScope::User,
        SourceScope::Project,
        SourceScope::Temporary,
    ] {
        let dirs = Dirs::new();
        let settings = SettingsManager::in_memory_json(
            serde_json::json!({
                "npmCommand": [node_join(&[&dirs.temp_dir, "missing-npm"])],
            })
            .as_object()
            .unwrap()
            .clone(),
            None,
        );
        let source = "npm:broken-package";
        match scope {
            SourceScope::User => {
                settings.set_packages(&[serde_json::from_value(serde_json::json!(source)).unwrap()])
            }
            SourceScope::Project => settings
                .set_project_packages(&[serde_json::from_value(serde_json::json!(source)).unwrap()])
                .unwrap(),
            SourceScope::Temporary => {}
        }
        write(
            &node_join(&[&dirs.agent_dir, "prompts", "healthy.md"]),
            "Healthy local prompt",
        );
        let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
            settings_manager: Some(settings),
            additional_extension_paths: (scope == SourceScope::Temporary)
                .then(|| vec![source.to_string()]),
            ..dirs.options()
        });
        for _ in 0..2 {
            loader.reload(None).await.unwrap();
            let extensions = loader.get_extensions();
            assert!(extensions.errors.is_empty());
            let warnings = extensions.warnings.unwrap();
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].path, source);
            assert!(warnings[0].warning.starts_with("failed to install ("));
            let hint = match scope {
                SourceScope::User => {
                    "Run \"pi remove npm:broken-package\" to remove it from settings.json."
                }
                SourceScope::Project => {
                    "Run \"pi remove npm:broken-package --local\" to remove it from settings.json."
                }
                SourceScope::Temporary => "Check the source and try again.",
            };
            assert!(
                warnings[0].warning.ends_with(hint),
                "{}",
                warnings[0].warning
            );
            assert!(
                loader
                    .get_prompts()
                    .prompts
                    .iter()
                    .any(|prompt| prompt.name == "healthy")
            );
        }
    }
}

#[tokio::test]
async fn package_install_failure_warns_once_across_project_trust_passes() {
    let _guard = crate::core::package_manager::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dirs = Dirs::new();
    let settings = SettingsManager::in_memory_json(
        serde_json::json!({
            "npmCommand": [node_join(&[&dirs.temp_dir, "missing-npm"])],
            "packages": ["npm:broken-package"],
        })
        .as_object()
        .unwrap()
        .clone(),
        None,
    );
    let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        settings_manager: Some(settings),
        ..dirs.options()
    });
    loader
        .reload(Some(&ResourceLoaderReloadOptions {
            resolve_project_trust: Some(Arc::new(|input| {
                Box::pin(async move {
                    let warnings = input.extensions_result.warnings.unwrap();
                    assert_eq!(warnings.len(), 1);
                    assert_eq!(warnings[0].path, "npm:broken-package");
                    Ok(true)
                })
            })),
        }))
        .await
        .unwrap();
    assert_eq!(loader.get_extensions().warnings.unwrap().len(), 1);
}

#[test]
fn package_install_failure_warning_has_upstream_format() {
    let failure = PackageInstallFailure {
        source: "npm:broken-package".to_string(),
        scope: SourceScope::Project,
        message: "registry unavailable".to_string(),
    };
    assert_eq!(
        format_package_install_warning(&failure),
        "Extension package \"npm:broken-package\": failed to install (registry unavailable). Run \"pi remove npm:broken-package --local\" to remove it from settings.json."
    );
}
