//! Golden (differential) tests of configuration and resource resolution against the TypeScript
//! reference: settings loading, merging, getters and writes; git URL, npm spec and local package
//! source parsing; package and resource resolution over file trees; skill, prompt template and
//! context file discovery; prompt template arguments; model reference and scope resolution against
//! the built-in catalog; keybinding resolution.
//!
//! `tests/fixtures/golden/config.json` is written by `scripts/golden/gen-config.mjs` (regenerate with
//! `PI_TS_REF=<pi checkout> scripts/golden/run.sh scripts/golden/gen-config.mjs`). File-tree cases
//! are rebuilt below a temporary root; `<root>` in the expected output stands for it.
//! `GOLDEN_FILTER=<substring>` limits a run. Lives in the crate (not `tests/`) because several
//! package manager helpers it checks are crate-private.

use std::path::Path;
use std::sync::Arc;

use futures::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::compat::WheelScrollLines;
use crate::core::keybindings::{KeybindingsManager, keybindings_config_to_json, migrate_keybindings_config};
use crate::core::model_resolver::{
    ModelReferenceRequest, ModelReferenceResult, resolve_model_reference, resolve_model_scope_from_models,
};
use crate::core::package_manager::{
    DefaultPackageManager, MissingSourceAction, MissingSourceHandler, PackageManager,
    PackageManagerAddSourceToSettingsOptions, PackageManagerOptions, PackageManagerRemoveSourceFromSettingsOptions,
    PackageManagerResolveExtensionSourcesOptions, ResolvedPaths, SourceScope,
};
use crate::core::prompt_templates::{
    LoadPromptTemplatesOptions, PromptTemplate, expand_prompt_template, load_prompt_templates, parse_command_args,
    substitute_args,
};
use crate::core::resource_loader::{LoadProjectContextFilesOptions, load_project_context_files};
use crate::core::settings_diagnostics::collect_settings_diagnostics;
use crate::core::settings_manager::{
    SettingsManager, SettingsManagerCreateOptions, SettingsManagerGetCompactionSettingsModel,
};
use crate::core::skills::{LoadSkillsOptions, load_skills};
use crate::core::source_info::{SourceInfo, SourceOrigin};
use crate::utils::git::parse_git_url;
use pi_ai::types::Model;
use pi_tui::components::scroll_view::ScrollViewScrollbar;
use pi_tui::terminal_image::ImageProtocol;

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    kind: String,
    input: Value,
    out: Value,
}

fn fixture() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden/config.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn selected<'a>(fixture: &'a Fixture, kind: &str) -> Vec<&'a Case> {
    let filter = std::env::var("GOLDEN_FILTER").unwrap_or_default();
    let cases: Vec<&Case> = fixture.cases.iter().filter(|c| c.kind == kind && c.name.contains(&filter)).collect();
    assert!(!cases.is_empty() || !filter.is_empty(), "no golden cases of kind {kind}");
    cases
}

/// `undefined` marker of the generator.
fn u(value: Option<Value>) -> Value {
    value.unwrap_or_else(|| json!("<undefined>"))
}

/// JSON numbers compare by value (`1` and `1.0` are the same JS number).
fn normalize(value: &Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => json!(f as i64),
            Some(f) => json!(f),
            None => value.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), normalize(v))).collect()),
        other => other.clone(),
    }
}

fn check(failures: &mut Vec<String>, name: &str, expected: &Value, actual: &Value) {
    let (expected, actual) = (normalize(expected), normalize(actual));
    if expected != actual {
        let mut detail = String::new();
        if let (Value::Object(e), Value::Object(a)) = (&expected, &actual) {
            for key in e.keys().chain(a.keys().filter(|k| !e.contains_key(*k))) {
                if let (Some(Value::Object(ee)), Some(Value::Object(aa))) = (e.get(key), a.get(key))
                    && key == "getters"
                {
                    for sub in ee.keys().chain(aa.keys().filter(|k| !ee.contains_key(*k))) {
                        if ee.get(sub) != aa.get(sub) {
                            detail.push_str(&format!(
                                "\n    {key}.{sub}:\n      ts: {}\n      rs: {}",
                                ee.get(sub).map(|v| v.to_string()).unwrap_or_else(|| "<missing>".into()),
                                aa.get(sub).map(|v| v.to_string()).unwrap_or_else(|| "<missing>".into())
                            ));
                        }
                    }
                    continue;
                }
                if e.get(key) != a.get(key) {
                    detail.push_str(&format!(
                        "\n    {key}:\n      ts: {}\n      rs: {}",
                        e.get(key).map(|v| v.to_string()).unwrap_or_else(|| "<missing>".into()),
                        a.get(key).map(|v| v.to_string()).unwrap_or_else(|| "<missing>".into())
                    ));
                }
            }
        } else {
            detail = format!("\n    ts: {expected}\n    rs: {actual}");
        }
        let mut paths = Vec::new();
        diff_paths("", &expected, &actual, &mut paths, 12);
        failures.push(format!("{name}:{detail}\n  differences:\n    {}", paths.join("\n    ")));
    }
}

/// Paths where two JSON values differ (at most `limit`).
fn diff_paths(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit || expected == actual {
        return;
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for key in e.keys().chain(a.keys().filter(|k| !e.contains_key(*k))) {
                let missing = Value::String("<missing>".into());
                diff_paths(&format!("{path}.{key}"), e.get(key).unwrap_or(&missing), a.get(key).unwrap_or(&missing), out, limit);
            }
        }
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => {
            for (i, (x, y)) in e.iter().zip(a).enumerate() {
                diff_paths(&format!("{path}[{i}]"), x, y, out, limit);
            }
        }
        _ => out.push(format!("{path}: ts {expected} / rs {actual}")),
    }
}

fn finish(kind: &str, failures: Vec<String>) {
    if !failures.is_empty() {
        panic!("{} {kind} golden case(s) differ:\n{}", failures.len(), failures.join("\n"));
    }
}

/// A temporary directory holding the case's file tree.
struct Tree {
    _dir: tempfile::TempDir,
    root: String,
}

impl Tree {
    fn new(files: Option<&Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().to_string_lossy().into_owned();
        if let Some(Value::Object(files)) = files {
            for (rel, content) in files {
                let file = Path::new(&root).join(rel.trim_end_matches('/'));
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                match content {
                    Value::Object(spec) if spec.contains_key("symlink") => {
                        std::os::unix::fs::symlink(spec["symlink"].as_str().unwrap(), &file).unwrap()
                    }
                    Value::Object(spec) if spec.contains_key("dir") => std::fs::create_dir_all(&file).unwrap(),
                    Value::String(text) => std::fs::write(&file, text).unwrap(),
                    other => panic!("unsupported file spec {other}"),
                }
            }
        }
        Self { _dir: dir, root }
    }

    fn path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root)
    }

    fn relativize(&self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(s.replace(&self.root, "<root>")),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.relativize(v)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, self.relativize(v))).collect()),
            other => other,
        }
    }
}

fn read_if_exists(path: &str) -> Value {
    json!(std::fs::read_to_string(path).unwrap_or_else(|_| "<missing>".to_string()))
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// `{ ok: value }` or `{ error: message }` like the generator's `capture`.
fn ok(value: Option<Value>) -> Value {
    json!({ "ok": u(value) })
}

fn ok_or<T>(result: anyhow::Result<T>, map: impl FnOnce(T) -> Option<Value>) -> Value {
    match result {
        Ok(value) => ok(map(value)),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Arrays are objects in JS; spreading one into settings keys it by index.
fn js_object(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            Value::Object(items.iter().enumerate().map(|(i, v)| (i.to_string(), v.clone())).collect())
        }
        other => other.clone(),
    }
}

// -------------------------------------------------------------------------------------------------
// Settings

fn settings_getters(s: &SettingsManager) -> Value {
    let model = SettingsManagerGetCompactionSettingsModel::new("anthropic", "claude-opus-4-8");
    let mut g = Map::new();
    let mut put = |name: &str, value: Value| {
        g.insert(name.to_string(), value);
    };
    put("getSessionDir", ok(s.get_session_dir().map(Value::from)));
    put("getDefaultProvider", ok(s.get_default_provider().map(Value::from)));
    put("getDefaultModel", ok(s.get_default_model().map(Value::from)));
    put("getSteeringMode", ok(Some(to_json(&s.get_steering_mode()))));
    put("getFollowUpMode", ok(Some(to_json(&s.get_follow_up_mode()))));
    put("getThemeSetting", ok(s.get_theme_setting().map(Value::from)));
    put("getTheme", ok(s.get_theme().map(Value::from)));
    put("getDefaultThinkingLevel", ok(s.get_default_thinking_level_setting().map(|l| to_json(&l))));
    put("getModelThinkingLevel", ok(s.get_model_thinking_level_setting("anthropic", "claude-opus-4-8").map(|l| to_json(&l))));
    put("getAllModelThinkingLevels", ok(Some(to_json(&s.get_all_model_thinking_level_settings()))));
    put("getTransport", ok(Some(to_json(&s.get_transport()))));
    put("getSessionShareDestination", ok(Some(to_json(&s.get_session_share_destination()))));
    put("getCompactionSettings", ok_or(s.get_compaction_settings(None), |v| Some(to_json(&v))));
    put("getCompactionSettingsForModel", ok_or(s.get_compaction_settings(Some(&model)), |v| Some(to_json(&v))));
    put("getBranchSummarySettings", ok(Some(to_json(&s.get_branch_summary_settings()))));
    put("getRetrySettings", ok(Some(to_json(&s.get_retry_settings()))));
    put("getHttpIdleTimeoutMs", ok_or(s.get_http_idle_timeout_ms(), |v| Some(json!(v))));
    put("getCacheWarmingMode", ok(Some(to_json(&s.get_cache_warming_mode()))));
    put("getProviderRetrySettings", ok(Some(to_json(&s.get_provider_retry_settings()))));
    put("getWebSocketConnectTimeoutMs", ok_or(s.get_web_socket_connect_timeout_ms(), |v| v.map(Value::from)));
    put("getHideThinkingBlock", ok(Some(json!(s.get_hide_thinking_block()))));
    put("getShowCacheMissNotices", ok(Some(json!(s.get_show_cache_miss_notices()))));
    put("getShellPath", ok(s.get_shell_path().map(Value::from)));
    put("getQuietStartup", ok(Some(to_json(&s.get_quiet_startup()))));
    put("getDefaultProjectTrust", ok(Some(to_json(&s.get_default_project_trust()))));
    put("getShellCommandPrefix", ok(s.get_shell_command_prefix().map(Value::from)));
    put("getNpmCommand", ok(s.get_npm_command().map(|c| to_json(&c))));
    put("getCollapseChangelog", ok(Some(json!(s.get_collapse_changelog()))));
    put("getEnableInstallTelemetry", ok(Some(json!(s.get_enable_install_telemetry()))));
    put("getEnableAnalytics", ok(Some(json!(s.get_enable_analytics()))));
    put("getPackages", ok(Some(to_json(&s.get_packages()))));
    put("getExtensionPaths", ok(Some(to_json(&s.get_extension_paths()))));
    put("getSkillPaths", ok(Some(to_json(&s.get_skill_paths()))));
    put("getPromptTemplatePaths", ok(Some(to_json(&s.get_prompt_template_paths()))));
    put("getThemePaths", ok(Some(to_json(&s.get_theme_paths()))));
    put("getEnableSkillCommands", ok(Some(json!(s.get_enable_skill_commands()))));
    put("getThinkingBudgets", ok(s.get_thinking_budgets().map(|b| to_json(&b))));
    let overrides = s.get_terminal_capability_overrides();
    let mut caps = Map::new();
    if let Some(images) = overrides.images {
        caps.insert(
            "images".into(),
            match images {
                Some(ImageProtocol::Kitty) => json!("kitty"),
                Some(ImageProtocol::Iterm2) => json!("iterm2"),
                None => Value::Null,
            },
        );
    }
    if let Some(true_color) = overrides.true_color {
        caps.insert("trueColor".into(), json!(true_color));
    }
    if let Some(hyperlinks) = overrides.hyperlinks {
        caps.insert("hyperlinks".into(), json!(hyperlinks));
    }
    put("getTerminalCapabilityOverrides", ok(Some(Value::Object(caps))));
    put("getShowImages", ok(Some(json!(s.get_show_images()))));
    put("getImageWidthCells", ok(Some(json!(s.get_image_width_cells()))));
    put("getClearOnShrink", ok(Some(json!(s.get_clear_on_shrink()))));
    put("getShowTerminalProgress", ok(Some(json!(s.get_show_terminal_progress()))));
    put("getTuiMode", ok(Some(to_json(&s.get_tui_mode()))));
    put("getFullscreenExitOutput", ok(Some(to_json(&s.get_fullscreen_exit_output()))));
    put(
        "getFullscreenScrollbar",
        ok(Some(json!(match s.get_fullscreen_scrollbar() {
            ScrollViewScrollbar::Auto => "auto",
            ScrollViewScrollbar::Always => "always",
            ScrollViewScrollbar::Hidden => "hidden",
        }))),
    );
    put("getFullscreenCopyOnSelect", ok(Some(json!(s.get_fullscreen_copy_on_select()))));
    put(
        "getFullscreenWheelScrollLines",
        ok(Some(match s.get_fullscreen_wheel_scroll_lines() {
            WheelScrollLines::Lines(n) => json!(n),
            WheelScrollLines::Auto(_) => json!("auto"),
        })),
    );
    put("getImageAutoResize", ok(Some(json!(s.get_image_auto_resize()))));
    put("getBlockImages", ok(Some(json!(s.get_block_images()))));
    put("getEnabledModels", ok(s.get_enabled_models().map(|m| to_json(&m))));
    put("getDefaultTools", ok(s.get_default_tools().map(|t| to_json(&t))));
    put("getDoubleEscapeAction", ok(Some(to_json(&s.get_double_escape_action()))));
    put("getTreeFilterMode", ok(Some(to_json(&s.get_tree_filter_mode()))));
    put("getShowHardwareCursor", ok(Some(json!(s.get_show_hardware_cursor()))));
    put("getEditorPaddingX", ok(Some(json!(s.get_editor_padding_x()))));
    put("getOutputPad", ok(Some(json!(s.get_output_pad()))));
    put("getAutocompleteMaxVisible", ok(Some(json!(s.get_autocomplete_max_visible()))));
    put("getCodeBlockIndent", ok(Some(json!(s.get_code_block_indent()))));
    put("getMermaidRenderingMode", ok(Some(to_json(&s.get_mermaid_rendering_mode()))));
    put("getWarnings", ok(Some(to_json(&s.get_warnings()))));
    Value::Object(g)
}

const BOOLEAN_GETTERS: &[&str] = &[
    "getHideThinkingBlock",
    "getShowCacheMissNotices",
    "getCollapseChangelog",
    "getEnableInstallTelemetry",
    "getEnableAnalytics",
    "getEnableSkillCommands",
    "getShowImages",
    "getClearOnShrink",
    "getShowTerminalProgress",
    "getFullscreenCopyOnSelect",
    "getImageAutoResize",
    "getBlockImages",
    "getShowHardwareCursor",
];

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn from_arg<T: serde::de::DeserializeOwned>(args: &[Value], index: usize) -> T {
    serde_json::from_value(args.get(index).cloned().unwrap_or(Value::Null)).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

fn run_settings_op(rt: &tokio::runtime::Runtime, s: &SettingsManager, op: &Value, agent_dir: &str) -> anyhow::Result<()> {
    let name = op["op"].as_str().unwrap();
    let args: Vec<Value> = op.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    let str_arg = |i: usize| args[i].as_str().unwrap().to_string();
    let floor = |i: usize| args[i].as_f64().unwrap().floor() as i64;
    match name {
        "writeGlobal" => std::fs::write(format!("{agent_dir}/settings.json"), str_arg(0))?,
        "reload" => rt.block_on(s.reload())?,
        "setProjectTrusted" => s.set_project_trusted(args[0].as_bool().unwrap()),
        "applyOverrides" => s.apply_overrides_json(args[0].as_object().unwrap()),
        "setDefaultModelAndProvider" => s.set_default_model_and_provider(&str_arg(0), &str_arg(1)),
        "setTheme" => s.set_theme(&str_arg(0)),
        "setCompactionEnabled" => s.set_compaction_enabled(args[0].as_bool().unwrap()),
        "setModelThinkingLevel" => s.set_model_thinking_level(&str_arg(0), &str_arg(1), from_arg(&args, 2)),
        "removeModelThinkingLevel" => s.remove_model_thinking_level(&str_arg(0), &str_arg(1)),
        "setShowImages" => s.set_show_images(args[0].as_bool().unwrap()),
        "setImageWidthCells" => s.set_image_width_cells(floor(0)),
        "setMermaidRenderingMode" => s.set_mermaid_rendering_mode(&from_arg(&args, 0)),
        "setPackages" => s.set_packages(&from_arg::<Vec<_>>(&args, 0)),
        "setEnabledModels" => s.set_enabled_models(args.first().filter(|v| !v.is_null()).map(strings).as_deref()),
        "setShellPath" => s.set_shell_path(args.first().and_then(Value::as_str)),
        "setHttpIdleTimeoutMs" => s.set_http_idle_timeout_ms(floor(0))?,
        "setFullscreenWheelScrollLines" => {
            s.set_fullscreen_wheel_scroll_lines(&WheelScrollLines::Lines(args[0].as_u64().unwrap().min(100) as u32))
        }
        "setEditorPaddingX" => s.set_editor_padding_x(floor(0)),
        "setAutocompleteMaxVisible" => s.set_autocomplete_max_visible(floor(0)),
        "setSteeringMode" => s.set_steering_mode(from_arg(&args, 0)),
        "setQuietStartup" => s.set_quiet_startup(&from_arg(&args, 0)),
        "setNpmCommand" => s.set_npm_command(Some(&strings(&args[0]))),
        "setWarnings" => s.set_warnings(&from_arg(&args, 0)),
        "setRetryEnabled" => s.set_retry_enabled(args[0].as_bool().unwrap()),
        "setBlockImages" => s.set_block_images(args[0].as_bool().unwrap()),
        "setClearOnShrink" => s.set_clear_on_shrink(args[0].as_bool().unwrap()),
        "setOutputPad" => s.set_output_pad(floor(0)),
        "setTreeFilterMode" => s.set_tree_filter_mode(from_arg(&args, 0)),
        "setDefaultProjectTrust" => s.set_default_project_trust(&from_arg(&args, 0)),
        "setProjectPackages" => s.set_project_packages(&from_arg::<Vec<_>>(&args, 0))?,
        "setProjectExtensionPaths" => s.set_project_extension_paths(&strings(&args[0]))?,
        "setProjectSkillPaths" => s.set_project_skill_paths(&strings(&args[0]))?,
        "setProjectThemePaths" => s.set_project_theme_paths(&strings(&args[0]))?,
        "setProjectPromptTemplatePaths" => s.set_project_prompt_template_paths(&strings(&args[0]))?,
        other => panic!("unknown settings op {other}"),
    }
    Ok(())
}

#[test]
fn golden_settings() {
    // `~` expands against HOME, which other golden tests swap while holding this lock.
    let _env = crate::core::package_manager::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let rt = runtime();
    let mut failures = Vec::new();
    for case in selected(&fixture, "settings") {
        let tree = Tree::new(case.input.get("files"));
        let cwd = tree.path("project");
        let agent_dir = tree.path("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(global) = case.input.get("global").and_then(Value::as_str) {
            std::fs::write(format!("{agent_dir}/settings.json"), global).unwrap();
        }
        if let Some(project) = case.input.get("project").and_then(Value::as_str) {
            std::fs::create_dir_all(format!("{cwd}/.pi")).unwrap();
            std::fs::write(format!("{cwd}/.pi/settings.json"), project).unwrap();
        }
        let trusted = case.input.get("trusted").and_then(Value::as_bool).unwrap_or(true);
        let s = SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            Some(&SettingsManagerCreateOptions { project_trusted: Some(trusted) }),
        );
        let mut ops = Vec::new();
        for op in case.input.get("ops").and_then(Value::as_array).cloned().unwrap_or_default() {
            ops.push(match run_settings_op(&rt, &s, &op, &agent_dir) {
                Ok(()) => json!("ok"),
                Err(error) => json!({ "error": error.to_string() }),
            });
        }
        let actual = json!({
            "settings": Value::Object(s.get_settings_json()),
            "globalSettings": Value::Object(s.get_global_settings_json()),
            "projectSettings": Value::Object(s.get_project_settings_json()),
            "getters": settings_getters(&s),
            "ops": ops,
            "diagnostics": collect_settings_diagnostics(s.clone()).into_iter().map(|d| d.message).collect::<Vec<_>>(),
            "globalFile": read_if_exists(&format!("{agent_dir}/settings.json")),
            "projectFile": read_if_exists(&format!("{cwd}/.pi/settings.json")),
        });
        let mut expected = case.out.clone();
        for key in ["globalSettings", "projectSettings"] {
            expected[key] = js_object(&expected[key]);
        }
        // Boolean getters return the raw value in TS (`setting ?? default`); callers test truthiness.
        if let Value::Object(getters) = &mut expected["getters"] {
            for getter in BOOLEAN_GETTERS {
                if let Some(value) = getters.get(*getter).and_then(|r| r.get("ok")).cloned()
                    && !value.is_boolean()
                {
                    getters[*getter] = json!({ "ok": js_truthy(&value) });
                }
            }
        }
        for (getter, field) in [
            ("getCompactionSettings", "enabled"),
            ("getCompactionSettingsForModel", "enabled"),
            ("getBranchSummarySettings", "skipPrompt"),
            ("getRetrySettings", "enabled"),
        ] {
            if let Some(value) = expected["getters"][getter]["ok"].get(field).cloned()
                && !value.is_boolean()
            {
                expected["getters"][getter]["ok"][field] = json!(js_truthy(&value));
            }
        }
        // `~` expands against the real home directory; the generator ran with HOME=/home/golden.
        let home = std::env::var("HOME").unwrap_or_default();
        let mut actual = actual;
        if let Some(Value::String(path)) = actual["getters"]["getShellPath"].get_mut("ok")
            && !home.is_empty()
            && path.starts_with(&home)
        {
            *path = path.replacen(&home, "/home/golden", 1);
        }
        check(&mut failures, &case.name, &expected, &tree.relativize(actual));
    }
    finish("settings", failures);
}

// -------------------------------------------------------------------------------------------------
// Sources

#[test]
fn golden_git_urls() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, "git_url") {
        let actual = parse_git_url(case.input["source"].as_str().unwrap()).map(|g| to_json(&g)).unwrap_or(Value::Null);
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("git_url", failures);
}

fn package_manager(cwd: &str, agent_dir: &str, settings: Arc<SettingsManager>) -> DefaultPackageManager {
    DefaultPackageManager::new(&PackageManagerOptions {
        cwd: cwd.to_string(),
        agent_dir: agent_dir.to_string(),
        settings_manager: settings,
        builtin_extensions: None,
    })
}

#[test]
fn golden_package_sources() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, "pm_source") {
        let input = &case.input;
        let pm = package_manager(
            input["cwd"].as_str().unwrap(),
            input["agentDir"].as_str().unwrap(),
            SettingsManager::in_memory(None, None),
        );
        let source = input["source"].as_str().unwrap();
        let actual = json!({
            "parsed": to_json(&pm.parse_source(source)),
            "identity": pm.get_package_identity(source, None).unwrap(),
            "identityUser": pm.get_package_identity(source, Some(SourceScope::User)).unwrap(),
            "identityProject": pm.get_package_identity(source, Some(SourceScope::Project)).unwrap(),
            "identityTemporary": pm.get_package_identity(source, Some(SourceScope::Temporary)).unwrap(),
            "settingsUser": pm.normalize_package_source_for_settings(source, SourceScope::User).unwrap(),
            "settingsProject": pm.normalize_package_source_for_settings(source, SourceScope::Project).unwrap(),
            "matchKey": pm.get_source_match_key_for_input(source),
        });
        check(&mut failures, &case.name, &case.out, &actual);
    }
    for case in selected(&fixture, "pm_no_match") {
        let input = &case.input;
        let pm = package_manager(
            input["cwd"].as_str().unwrap(),
            input["agentDir"].as_str().unwrap(),
            SettingsManager::in_memory(None, None),
        );
        let configured = serde_json::from_value::<Vec<_>>(input["configured"].clone()).unwrap();
        let actual = json!(pm.build_no_matching_package_message(input["source"].as_str().unwrap(), &configured));
        check(&mut failures, &case.name, &case.out, &actual);
    }
    for case in selected(&fixture, "pm_npm_command") {
        let input = &case.input;
        let mut settings = Map::new();
        if let Value::Array(_) = &input["npmCommand"] {
            settings.insert("npmCommand".into(), input["npmCommand"].clone());
        }
        let pm = package_manager(
            input["cwd"].as_str().unwrap(),
            input["agentDir"].as_str().unwrap(),
            SettingsManager::in_memory_json(settings, None),
        );
        let specs = vec!["a".to_string(), "b@1".to_string()];
        let actual = json!({
            "name": ok_or(pm.get_package_manager_name(), |v| Some(json!(v))),
            "installArgs": ok_or(pm.get_npm_install_args(&specs, "/root/x"), |v| Some(json!(v))),
            "gitDependencyArgs": ok_or(pm.get_git_dependency_install_args(), |v| Some(json!(v))),
        });
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("package source", failures);
}

// -------------------------------------------------------------------------------------------------
// Resolution over file trees

fn shape_resolved(tree: &Tree, resolved: &ResolvedPaths) -> Value {
    let keep = |resources: &Vec<crate::core::package_manager::ResolvedResource>| -> Value {
        Value::Array(
            resources
                .iter()
                .filter(|r| r.path.starts_with(&tree.root) || r.path.starts_with("builtin:"))
                .map(to_json)
                .collect(),
        )
    };
    json!({
        "extensions": keep(&resolved.extensions),
        "skills": keep(&resolved.skills),
        "prompts": keep(&resolved.prompts),
        "themes": keep(&resolved.themes),
    })
}

#[test]
fn golden_package_resolution() {
    let fixture = fixture();
    let rt = runtime();
    let mut failures = Vec::new();
    for case in selected(&fixture, "pm_resolve") {
        let input = &case.input;
        let tree = Tree::new(input.get("files"));
        let cwd = tree.path("project");
        let agent_dir = tree.path("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(global) = input.get("global") {
            std::fs::write(format!("{agent_dir}/settings.json"), global.to_string()).unwrap();
        }
        if let Some(project) = input.get("project") {
            std::fs::create_dir_all(format!("{cwd}/.pi")).unwrap();
            std::fs::write(format!("{cwd}/.pi/settings.json"), project.to_string()).unwrap();
        }
        let trusted = input.get("trusted").and_then(Value::as_bool).unwrap_or(true);
        let settings = SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            Some(&SettingsManagerCreateOptions { project_trusted: Some(trusted) }),
        );
        let pm = DefaultPackageManager::new(&PackageManagerOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            settings_manager: settings.clone(),
            builtin_extensions: input.get("builtins").map(strings),
        });
        let skip: MissingSourceHandler = Arc::new(|_| async { Ok(MissingSourceAction::Skip) }.boxed());
        let mut out = Map::new();
        out.insert(
            "resolved".into(),
            match rt.block_on(pm.resolve(Some(skip))) {
                Ok(resolved) => shape_resolved(&tree, &resolved),
                Err(error) => json!({ "error": error.to_string() }),
            },
        );
        if let Some(sources) = input.get("cliSources") {
            let options = PackageManagerResolveExtensionSourcesOptions { local: None, temporary: Some(true) };
            out.insert(
                "cli".into(),
                match rt.block_on(pm.resolve_extension_sources(&strings(sources), Some(&options))) {
                    Ok(resolved) => shape_resolved(&tree, &resolved),
                    Err(error) => json!({ "error": error.to_string() }),
                },
            );
        }
        out.insert("configured".into(), to_json(&pm.list_configured_packages()));
        if let Some(ops) = input.get("ops").and_then(Value::as_array) {
            let mut results = Vec::new();
            for op in ops {
                let source = op["source"].as_str().unwrap();
                let local = op.get("local").and_then(Value::as_bool);
                let result = if op["op"] == "add" {
                    pm.add_source_to_settings(source, Some(&PackageManagerAddSourceToSettingsOptions { local }))
                } else {
                    pm.remove_source_from_settings(source, Some(&PackageManagerRemoveSourceFromSettingsOptions { local }))
                };
                results.push(ok_or(result, |v| Some(json!(v)))["ok"].clone());
            }
            out.insert("ops".into(), Value::Array(results));
            out.insert("globalFile".into(), read_if_exists(&format!("{agent_dir}/settings.json")));
            out.insert("projectFile".into(), read_if_exists(&format!("{cwd}/.pi/settings.json")));
        }
        check(&mut failures, &case.name, &case.out, &tree.relativize(Value::Object(out)));
    }
    finish("pm_resolve", failures);
}

#[test]
fn golden_resource_discovery() {
    let fixture = fixture();
    let mut failures = Vec::new();
    // YAML syntax errors are reported by different parsers (the `yaml` npm package in TS,
    // yaml-rust2 here), so only their presence and location are compared.
    fn mask_yaml_errors(expected: &mut Value, actual: &mut Value) {
        let (Some(e), Some(a)) = (expected["diagnostics"].as_array_mut(), actual["diagnostics"].as_array_mut()) else {
            return;
        };
        for (e, a) in e.iter_mut().zip(a.iter_mut()) {
            let ts = e["message"].as_str().unwrap_or("");
            let rs = a["message"].as_str().unwrap_or("");
            if ts != rs && ts.contains(" at line ") && rs.contains(" at line ") && e["path"] == a["path"] {
                e["message"] = json!("<yaml error>");
                a["message"] = json!("<yaml error>");
            }
        }
    }
    for case in selected(&fixture, "skills") {
        let input = &case.input;
        let tree = Tree::new(input.get("files"));
        std::fs::create_dir_all(tree.path("project")).unwrap();
        std::fs::create_dir_all(tree.path("agent")).unwrap();
        let result = load_skills(&LoadSkillsOptions {
            cwd: tree.path("project"),
            agent_dir: tree.path("agent"),
            skill_paths: input.get("skillPaths").map(strings).unwrap_or_default(),
            include_defaults: input.get("includeDefaults").and_then(Value::as_bool).unwrap_or(true),
        });
        let (mut expected, mut actual) = (case.out.clone(), tree.relativize(to_json(&result)));
        mask_yaml_errors(&mut expected, &mut actual);
        check(&mut failures, &case.name, &expected, &actual);
    }
    for case in selected(&fixture, "prompts") {
        let input = &case.input;
        let tree = Tree::new(input.get("files"));
        std::fs::create_dir_all(tree.path("project")).unwrap();
        std::fs::create_dir_all(tree.path("agent")).unwrap();
        let result = load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: tree.path("project"),
            agent_dir: tree.path("agent"),
            prompt_paths: input.get("promptPaths").map(strings).unwrap_or_default(),
            include_defaults: input.get("includeDefaults").and_then(Value::as_bool).unwrap_or(true),
        });
        let (mut expected, mut actual) = (case.out.clone(), tree.relativize(to_json(&result)));
        mask_yaml_errors(&mut expected, &mut actual);
        check(&mut failures, &case.name, &expected, &actual);
    }
    for case in selected(&fixture, "context_files") {
        let input = &case.input;
        let tree = Tree::new(input.get("files"));
        let cwd = tree.path(input.get("cwd").and_then(Value::as_str).unwrap_or("project"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(tree.path("agent")).unwrap();
        let files: Vec<Value> = load_project_context_files(&LoadProjectContextFilesOptions { cwd, agent_dir: tree.path("agent") })
            .into_iter()
            .filter(|file| file.path.starts_with(&tree.root))
            .map(|file| to_json(&file))
            .collect();
        // The golden file was generated on a case-sensitive filesystem. On a case-insensitive one the
        // `AGENTS.md` / `CLAUDE.md` candidates match their `.MD` spellings first, as in upstream.
        let mut expected = case.out.clone();
        if std::fs::metadata(tree.path("AGENT")).is_ok() {
            for file in expected.as_array_mut().into_iter().flatten() {
                if let Some(path) = file["path"].as_str().filter(|p| p.ends_with("/AGENTS.MD") || p.ends_with("/CLAUDE.MD")) {
                    file["path"] = json!(format!("{}.md", path.strip_suffix(".MD").unwrap()));
                }
            }
        }
        check(&mut failures, &case.name, &expected, &tree.relativize(Value::Array(files)));
    }
    finish("resource discovery", failures);
}

// -------------------------------------------------------------------------------------------------
// Prompt template arguments

#[test]
fn golden_prompt_arguments() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, "parse_args") {
        let actual = to_json(&parse_command_args(case.input["input"].as_str().unwrap()));
        check(&mut failures, &case.name, &case.out, &actual);
    }
    for case in selected(&fixture, "substitute") {
        let actual = json!(substitute_args(case.input["template"].as_str().unwrap(), &strings(&case.input["args"])));
        check(&mut failures, &case.name, &case.out, &actual);
    }
    for case in selected(&fixture, "expand") {
        let templates: Vec<PromptTemplate> = case.input["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| PromptTemplate {
                name: t["name"].as_str().unwrap().to_string(),
                description: String::new(),
                argument_hint: None,
                content: t["content"].as_str().unwrap().to_string(),
                source_info: SourceInfo {
                    path: String::new(),
                    source: "local".into(),
                    scope: SourceScope::Temporary,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                },
                file_path: String::new(),
            })
            .collect();
        let actual = json!(expand_prompt_template(case.input["text"].as_str().unwrap(), &templates));
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("prompt arguments", failures);
}

// -------------------------------------------------------------------------------------------------
// Models

fn catalog() -> Vec<Model> {
    pi_ai::providers::all::get_builtin_providers()
        .iter()
        .flat_map(|provider| pi_ai::providers::all::get_builtin_models(provider))
        .collect()
}

fn key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn shape_reference(result: &ModelReferenceResult) -> Value {
    match result {
        ModelReferenceResult::Resolved { model, thinking_level, warning } => json!({
            "status": "resolved",
            "model": key(model),
            "thinkingLevel": u(thinking_level.map(|l| to_json(&l))),
            "warning": u(warning.clone().map(Value::from)),
        }),
        ModelReferenceResult::Ambiguous { candidates, usable_count, thinking_level, warning } => json!({
            "status": "ambiguous",
            "candidates": candidates.iter().map(key).collect::<Vec<_>>(),
            "usableCount": usable_count,
            "thinkingLevel": u(thinking_level.map(|l| to_json(&l))),
            "warning": u(warning.clone().map(Value::from)),
        }),
        ModelReferenceResult::NotFound { target: None } => json!({ "status": "not-found" }),
        ModelReferenceResult::NotFound { target: Some(target) } => json!({
            "status": "not-found",
            "target": {
                "provider": target.provider,
                "providerModels": target.provider_models.iter().map(key).collect::<Vec<_>>(),
                "modelText": target.model_text,
                "modelId": target.model_id,
                "thinkingLevel": u(target.thinking_level.map(|l| to_json(&l))),
            },
        }),
    }
}

#[test]
fn golden_model_resolution() {
    let fixture = fixture();
    let models = catalog();
    let mut failures = Vec::new();
    for case in selected(&fixture, "model_reference") {
        let request: ModelReferenceRequest = serde_json::from_value(case.input["request"].clone()).unwrap();
        let usable_providers: Option<Vec<String>> = case.input["usable"].as_array().map(|_| strings(&case.input["usable"]));
        let usable = |provider: &str| usable_providers.as_ref().is_none_or(|list| list.iter().any(|p| p == provider));
        let actual = shape_reference(&resolve_model_reference(&request, &models, &usable));
        check(&mut failures, &case.name, &case.out, &actual);
    }
    for case in selected(&fixture, "model_scope") {
        let result = resolve_model_scope_from_models(&strings(&case.input["patterns"]), &models);
        let actual = json!({
            "scopedModels": result.scoped_models.iter().map(|scoped| json!({
                "model": key(&scoped.model),
                "thinkingLevel": u(scoped.thinking_level.map(|l| to_json(&l))),
            })).collect::<Vec<_>>(),
            "diagnostics": to_json(&result.diagnostics),
        });
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("model resolution", failures);
}

// -------------------------------------------------------------------------------------------------
// Keybindings

/// The golden file is generated on Linux, where the tree fold/unfold defaults are
/// `[ctrl+<dir>, alt+<dir>]`; upstream orders them `[alt+<dir>, ctrl+<dir>]` on darwin.
fn darwin_keybinding_defaults(mut expected: Value) -> Value {
    if cfg!(target_os = "macos")
        && let Some(effective) = expected.get_mut("effective").and_then(Value::as_object_mut)
    {
        for (key, dir) in [("app.tree.foldOrUp", "left"), ("app.tree.unfoldOrDown", "right")] {
            if effective.get(key) == Some(&json!([format!("ctrl+{dir}"), format!("alt+{dir}")])) {
                effective.insert(key.into(), json!([format!("alt+{dir}"), format!("ctrl+{dir}")]));
            }
        }
    }
    expected
}

#[test]
fn golden_keybindings() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, "keybindings") {
        let content = case.input["content"].as_str().filter(|c| *c != "<undefined>");
        let mut files = Map::new();
        if let Some(content) = content {
            files.insert("agent/keybindings.json".into(), json!(content));
        }
        let tree = Tree::new(Some(&Value::Object(files)));
        std::fs::create_dir_all(tree.path("agent")).unwrap();
        let manager = KeybindingsManager::create(Some(&tree.path("agent")));
        let migrated = match serde_json::from_str::<Value>(content.unwrap_or("{}").trim_start_matches('\u{feff}')) {
            Ok(Value::Object(map)) => to_json(&migrate_keybindings_config(map.into_iter().collect())),
            Ok(Value::Array(items)) => to_json(&migrate_keybindings_config(
                items.into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect(),
            )),
            Ok(_) => json!("<not object>"),
            Err(_) => json!("<invalid json>"),
        };
        let actual = json!({
            "effective": Value::Object(keybindings_config_to_json(&manager.get_effective_config())),
            "conflicts": manager.get_conflicts().into_iter().map(|c| json!({ "key": c.key, "keybindings": c.keybindings })).collect::<Vec<_>>(),
            "migrated": migrated,
        });
        check(&mut failures, &case.name, &darwin_keybinding_defaults(case.out.clone()), &actual);
    }
    finish("keybindings", failures);
}

// -------------------------------------------------------------------------------------------------
// models.json

#[test]
fn golden_models_json() {
    let fixture = fixture();
    let rt = runtime();
    let mut failures = Vec::new();
    for case in selected(&fixture, "model_config") {
        let mut files = Map::new();
        files.insert("models.json".into(), case.input["content"].clone());
        let tree = Tree::new(Some(&Value::Object(files)));
        let config = rt.block_on(crate::core::model_config::ModelConfig::load(Some(&tree.path("models.json"))));
        let ids = config.get_provider_ids();
        let providers: Map<String, Value> = ids
            .iter()
            .map(|id| {
                let provider = config.get_provider(id).unwrap();
                (
                    id.clone(),
                    json!({
                        "baseUrl": u(provider.base_url.clone().map(Value::from)),
                        "api": u(provider.api.clone().map(Value::from)),
                        "models": provider.models.iter().flatten().map(|m| m.id.clone()).collect::<Vec<_>>(),
                        "overrides": provider.model_overrides.iter().flat_map(|o| o.keys().cloned()).collect::<Vec<_>>(),
                    }),
                )
            })
            .collect();
        let actual = json!({
            "error": u(config.get_error().map(Value::from)),
            "providerIds": ids,
            "providers": providers,
        });
        check(&mut failures, &case.name, &case.out, &tree.relativize(actual));
    }
    finish("models.json", failures);
}

#[test]
fn golden_provider_composition() {
    let fixture = fixture();
    let builtins: std::collections::HashMap<String, Arc<dyn pi_ai::models::Provider>> =
        pi_ai::providers::all::builtin_providers().into_iter().map(|p| (p.id().to_string(), p)).collect();
    let mut failures = Vec::new();
    for case in selected(&fixture, "compose") {
        let provider_id = case.input["providerId"].as_str().unwrap();
        let config = Arc::new(crate::core::model_config::ModelConfig::from_content(
            case.input["content"].as_str().unwrap(),
            "models.json",
        ));
        let actual = match crate::core::provider_composer::compose_model_provider(
            provider_id,
            builtins.get(provider_id).cloned(),
            config,
            None,
        ) {
            Ok(provider) => json!({
                "name": provider.name(),
                "baseUrl": u(provider.base_url().map(Value::from)),
                "models": provider.get_models().iter().map(|model| {
                    let value = to_json(model);
                    let mut out = Map::new();
                    for key in ["id", "name", "api", "baseUrl", "reasoning", "input", "contextWindow", "maxTokens", "cost"] {
                        out.insert(key.into(), value.get(key).cloned().unwrap_or(Value::Null));
                    }
                    for key in ["compat", "thinkingLevelMap", "samplingParams", "samplingParamsByThinkingLevel", "promptCache", "inputLimits"] {
                        out.insert(key.into(), u(value.get(key).cloned().filter(|v| !v.is_null())));
                    }
                    Value::Object(out)
                }).collect::<Vec<_>>(),
            }),
            Err(error) => json!({ "error": error.to_string() }),
        };
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("provider composition", failures);
}

#[test]
fn golden_resource_loader() {
    use crate::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
    // The generator ran with an empty home; the real ~/.agents/skills would leak into the output.
    let _home = crate::core::package_manager::IsolatedHome::new();
    let fixture = fixture();
    let rt = runtime();
    let mut failures = Vec::new();
    for case in selected(&fixture, "resource_loader") {
        let input = &case.input;
        let tree = Tree::new(input.get("files"));
        let cwd = tree.path("project");
        let agent_dir = tree.path("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let trusted = input.get("trusted").and_then(Value::as_bool).unwrap_or(true);
        let settings = SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            Some(&SettingsManagerCreateOptions { project_trusted: Some(trusted) }),
        );
        let flag = |key: &str| input.get(key).and_then(Value::as_bool);
        let loader = DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            settings_manager: Some(settings),
            no_extensions: Some(true),
            additional_skill_paths: input.get("skillPaths").map(strings),
            additional_prompt_template_paths: input.get("promptPaths").map(strings),
            additional_theme_paths: input.get("themePaths").map(strings),
            no_skills: flag("noSkills"),
            no_prompt_templates: flag("noPrompts"),
            no_themes: flag("noThemes"),
            no_context_files: flag("noContextFiles"),
            system_prompt: input.get("systemPrompt").and_then(Value::as_str).map(str::to_string),
            append_system_prompt: input.get("appendSystemPrompt").map(strings),
            ..Default::default()
        });
        rt.block_on(loader.reload(None)).unwrap();
        let skills = loader.get_skills();
        let prompts = loader.get_prompts();
        let themes = loader.get_themes();
        let actual = json!({
            "skills": skills.skills.iter().map(|s| json!({ "name": s.name, "filePath": s.file_path, "sourceInfo": to_json(&s.source_info) })).collect::<Vec<_>>(),
            "skillDiagnostics": to_json(&skills.diagnostics),
            "prompts": prompts.prompts.iter().map(|p| json!({ "name": p.name, "filePath": p.file_path, "sourceInfo": to_json(&p.source_info) })).collect::<Vec<_>>(),
            "promptDiagnostics": to_json(&prompts.diagnostics),
            "themes": themes.themes.iter().map(|t| json!({
                "name": u(t.name().map(Value::from)),
                "sourcePath": u(t.source_path().map(Value::from)),
                "sourceInfo": u(t.source_info().map(|i| to_json(&i))),
            })).collect::<Vec<_>>(),
            "themeDiagnostics": to_json(&themes.diagnostics),
            "agentsFiles": loader.get_agents_files().agents_files.iter().filter(|f| f.path.starts_with(&tree.root)).map(to_json).collect::<Vec<_>>(),
            "systemPrompt": u(loader.get_system_prompt().map(Value::from)),
            "systemPromptSource": u(loader.get_system_prompt_source().map(|s| to_json(&s))),
            "appendSystemPrompt": loader.get_append_system_prompt(),
            "appendSystemPromptSources": to_json(&loader.get_append_system_prompt_sources()),
        });
        check(&mut failures, &case.name, &case.out, &tree.relativize(actual));
    }
    finish("resource loader", failures);
}

#[test]
fn golden_request_auth_status() {
    use crate::core::provider_composer::{configured_request_auth_status, resolve_compatibility_request_config};
    // SAFETY: test-only variables with names no other code reads.
    unsafe {
        std::env::set_var("PI_GOLDEN_SET_KEY", "secret");
        std::env::set_var("PI_GOLDEN_EMPTY_KEY", "");
        std::env::remove_var("PI_GOLDEN_UNSET_KEY");
    }
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in selected(&fixture, "auth_status") {
        let config = crate::core::model_config::ModelConfig::from_content(case.input["content"].as_str().unwrap(), "models.json");
        let provider = config.get_provider("p");
        let mut model = json!({
            "id": "m", "name": "m", "provider": "p", "api": "openai-completions", "baseUrl": "", "reasoning": false,
            "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1, "maxTokens": 1,
        });
        if case.name != "auth/modelHeaders" {
            model["headers"] = json!({ "x-model-own": "1" });
        }
        let model = pi_ai::types::AnyModel::from_value(model).unwrap();
        let actual = json!({
            "error": u(config.get_error().map(Value::from)),
            "status": u(configured_request_auth_status(provider, None).map(|s| to_json(&s))),
            "compat": ok_or(resolve_compatibility_request_config(&model, provider, None), |c| Some(to_json(&c))),
        });
        check(&mut failures, &case.name, &case.out, &actual);
    }
    finish("auth status", failures);
}
