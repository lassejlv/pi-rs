//! Lifecycle scripts, run like npm (`@npmcli/run-script` driven by arborist's `rebuild`).
//!
//! - Dependency scripts: `preinstall` for every new package, then bin links, then `install`, then
//!   `postinstall`; linked packages (`file:` directories, workspaces) also run `prepare` before
//!   their bins are linked. A package without `install`/`preinstall` scripts but with a
//!   `binding.gyp` gets npm's default `install` script, `node-gyp rebuild`.
//! - Queues are ordered like arborist (`depth`, then the path compared with `localeCompare`);
//!   npm runs a queue in parallel, the port runs it in that order.
//! - Scripts run through the system shell (`sh -c` on Unix, `%ComSpec% /d /s /c` on Windows, or
//!   the `script-shell` config) with npm's environment: the inherited environment, the npm config
//!   values from `.npmrc` as `npm_config_*`, `npm_package_*` (name, version, config, engines, bin),
//!   `npm_lifecycle_event`/`npm_lifecycle_script`, `npm_package_json`, `npm_package_resolved`,
//!   `npm_package_integrity`, the dependency flags, `INIT_CWD`, and `PATH` with every
//!   `node_modules/.bin` from the package folder up to the file system root in front. `NODE` and
//!   `npm_node_execpath` point at the `node` found on `PATH`, if any: like npm, scripts that call
//!   `node` need a Node.js installation.
//! - Dependency scripts have their output captured; a failure reports npm's error block (code,
//!   path, command, output). Root project scripts (`npm install` without arguments) inherit the
//!   terminal and print npm's `> name@version event` banner.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// npm's root lifecycle for `npm install` without package arguments.
pub(super) const ROOT_INSTALL_EVENTS: &[&str] = &[
    "preinstall",
    "install",
    "postinstall",
    "prepublish",
    "preprepare",
    "prepare",
    "postprepare",
];

pub(super) const DEFAULT_GYP_INSTALL_SCRIPT: &str = "node-gyp rebuild";

/// Environment shared by every script of one install.
#[derive(Debug, Clone)]
pub(super) struct ScriptEnv {
    /// `npm_config_*` and the other variables npm exports at startup.
    pub vars: Vec<(String, String)>,
    /// The `script-shell` config.
    pub script_shell: Option<String>,
}

impl ScriptEnv {
    /// Build npm's startup environment for an install rooted at `local_prefix`.
    pub fn new(local_prefix: &Path, npmrc: &[(String, String)], cli: &[(&str, &str)]) -> Self {
        let mut vars: Vec<(String, String)> = Vec::new();
        let mut set = |key: String, value: String| {
            vars.retain(|(k, _)| *k != key);
            vars.push((key, value));
        };
        let mut script_shell = None;
        for (key, value) in npmrc.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain(cli.iter().copied()) {
            if key == "script-shell" && !value.is_empty() {
                script_shell = Some(value.to_string());
            }
            if key.starts_with(['/', '@', '_']) || key.contains(":_") {
                continue;
            }
            set(
                format!("npm_config_{}", key.replace('-', "_").to_lowercase()),
                value.to_string(),
            );
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| local_prefix.to_path_buf());
        set("INIT_CWD".into(), cwd.to_string_lossy().into_owned());
        set("npm_command".into(), "install".into());
        set(
            "npm_config_local_prefix".into(),
            local_prefix.to_string_lossy().into_owned(),
        );
        set(
            "npm_config_user_agent".into(),
            format!(
                "pi/{} {} {} workspaces/false",
                crate::config::VERSION,
                super::current_os(),
                super::current_cpu()
            ),
        );
        if let Some(home) = dirs::home_dir() {
            let cache = if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA")
                    .map(|d| PathBuf::from(d).join("npm-cache"))
                    .unwrap_or_else(|| home.join("npm-cache"))
            } else {
                home.join(".npm")
            };
            set("npm_config_cache".into(), cache.to_string_lossy().into_owned());
        }
        if let Some(node) = crate::core::package_manager::find_executable("node") {
            let node = node.to_string_lossy().into_owned();
            if let Some(prefix) = Path::new(&node).parent().and_then(Path::parent) {
                let prefix = if cfg!(windows) {
                    Path::new(&node).parent().unwrap_or(prefix)
                } else {
                    prefix
                };
                set(
                    "npm_config_global_prefix".into(),
                    prefix.to_string_lossy().into_owned(),
                );
            }
            set("npm_node_execpath".into(), node.clone());
            set("NODE".into(), node);
        }
        set("COLOR".into(), "0".into());
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| if cfg!(windows) { "notepad.exe" } else { "vi" }.to_string());
        set("EDITOR".into(), editor);
        Self { vars, script_shell }
    }
}

/// One package whose scripts may run.
#[derive(Debug, Clone)]
pub(super) struct ScriptTarget {
    /// The folder the scripts run in (the link target for links).
    pub path: PathBuf,
    /// Where the package sits in the tree (sorting).
    pub location: PathBuf,
    pub depth: usize,
    pub manifest: Value,
    pub resolved: Option<String>,
    pub integrity: Option<String>,
    pub optional: bool,
    pub dev: bool,
}

/// A failed script, rendered like npm's error output.
#[derive(Debug)]
pub(super) struct ScriptFailure {
    pub message: String,
}

impl std::fmt::Display for ScriptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ScriptFailure {}

/// The script for `event`, including npm's implicit `node-gyp rebuild` install script.
pub(super) fn script_for(manifest: &Value, path: &Path, event: &str) -> Option<String> {
    let scripts = manifest.get("scripts").and_then(Value::as_object);
    let get = |name: &str| {
        scripts
            .and_then(|s| s.get(name))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(script) = get(event) {
        return Some(script);
    }
    if event == "install"
        && get("install").is_none()
        && get("preinstall").is_none()
        && manifest.get("gypfile") != Some(&Value::Bool(false))
        && path.join("binding.gyp").is_file()
    {
        return Some(DEFAULT_GYP_INSTALL_SCRIPT.to_string());
    }
    None
}

/// `packageEnvs`: `npm_package_<name|version|config|engines|bin>` flattened with `_`.
fn package_envs(manifest: &Value, out: &mut Vec<(String, String)>) {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Null | Value::Bool(false) => out.push((prefix.to_string(), String::new())),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, &format!("{prefix}_{index}"), out);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    walk(item, &format!("{prefix}_{key}"), out);
                }
            }
            Value::String(s) => out.push((prefix.to_string(), s.clone())),
            Value::Bool(true) => out.push((prefix.to_string(), "true".to_string())),
            Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        }
    }
    for key in ["name", "version", "config", "engines", "bin"] {
        if let Some(value) = manifest.get(key) {
            walk(value, &format!("npm_package_{key}"), out);
        }
    }
}

/// npm's `setPATH`: `<dir>/node_modules/.bin` for the folder and every ancestor, then `PATH`.
fn script_path(path: &Path) -> String {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut parts: Vec<String> = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        parts.push(dir.join("node_modules").join(".bin").to_string_lossy().into_owned());
        current = dir.parent();
    }
    let mut seen: Vec<String> = Vec::new();
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().eq_ignore_ascii_case("path") {
            for entry in value.to_string_lossy().split(separator) {
                if !seen.iter().any(|s| s == entry) {
                    seen.push(entry.to_string());
                }
            }
        }
    }
    parts.push(seen.join(separator));
    parts.join(separator)
}

fn shell_command(env: &ScriptEnv, script: &str) -> (String, Vec<String>) {
    if let Some(shell) = &env.script_shell {
        return (shell.clone(), vec!["-c".into(), script.to_string()]);
    }
    if cfg!(windows) {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        return (
            comspec,
            vec!["/d".into(), "/s".into(), "/c".into(), script.to_string()],
        );
    }
    ("sh".to_string(), vec!["-c".into(), script.to_string()])
}

fn bool_env(value: bool) -> String {
    if value { "1".into() } else { String::new() }
}

/// Build the command for one script.
fn build_command(
    env: &ScriptEnv,
    path: &Path,
    manifest: &Value,
    event: &str,
    script: &str,
    extra: &[(String, String)],
) -> (tokio::process::Command, String, Vec<String>) {
    let (shell, args) = shell_command(env, script);
    let mut command = tokio::process::Command::new(&shell);
    command.args(&args).current_dir(path);
    for (key, value) in &env.vars {
        command.env(key, value);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    let mut package = Vec::new();
    package_envs(manifest, &mut package);
    for (key, value) in package {
        command.env(key, value);
    }
    command
        .env("npm_package_json", path.join("package.json"))
        .env("npm_lifecycle_event", event)
        .env("npm_lifecycle_script", script);
    let path_value = script_path(path);
    let mut path_key_set = false;
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().eq_ignore_ascii_case("path") {
            command.env(&key, &path_value);
            path_key_set = true;
        }
    }
    if !path_key_set {
        command.env("PATH", &path_value);
    }
    (command, shell, args)
}

fn failure_message(
    code: Option<i32>,
    signal: Option<String>,
    path: &Path,
    shell: &str,
    args: &[String],
    stdout: &str,
    stderr: &str,
) -> String {
    let mut lines = Vec::new();
    match (code, signal) {
        (_, Some(signal)) => lines.push(format!("npm error signal {signal}")),
        (Some(code), None) => lines.push(format!("npm error code {code}")),
        (None, None) => lines.push("npm error code 1".to_string()),
    }
    lines.push(format!("npm error path {}", path.display()));
    lines.push("npm error command failed".to_string());
    lines.push(format!("npm error command {shell} {}", args.join(" ")));
    for output in [stdout, stderr] {
        let trimmed = output.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        for line in trimmed.split('\n') {
            lines.push(format!("npm error {line}"));
        }
    }
    lines.join("\n")
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(crate::core::package_manager::signal_name)
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Run one dependency script with captured output (arborist `rebuild`).
pub(super) async fn run_dependency_script(
    env: &ScriptEnv,
    target: &ScriptTarget,
    event: &str,
) -> Result<(), ScriptFailure> {
    let Some(script) = script_for(&target.manifest, &target.path, event) else {
        return Ok(());
    };
    let mut extra: Vec<(String, String)> = Vec::new();
    if let Some(resolved) = &target.resolved {
        extra.push(("npm_package_resolved".into(), resolved.clone()));
    }
    if let Some(integrity) = &target.integrity {
        extra.push(("npm_package_integrity".into(), integrity.clone()));
    }
    extra.push(("npm_package_optional".into(), bool_env(target.optional)));
    extra.push(("npm_package_dev".into(), bool_env(target.dev)));
    extra.push(("npm_package_peer".into(), String::new()));
    extra.push(("npm_package_dev_optional".into(), String::new()));
    let (mut command, shell, args) =
        build_command(env, &target.path, &target.manifest, event, &script, &extra);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    tracing::info!(
        "run {} {event} {} {script}",
        package_id(&target.manifest),
        target.location.display()
    );
    let output = match command.output().await {
        Ok(output) => output,
        Err(error) => {
            return Err(ScriptFailure {
                message: format!(
                    "npm error code {}\nnpm error syscall spawn {shell}\nnpm error path {}\nnpm error errno -2\nnpm error spawn {shell} {}",
                    error
                        .raw_os_error()
                        .map(|_| "ENOENT".to_string())
                        .unwrap_or_else(|| "1".to_string()),
                    target.path.display(),
                    error.kind()
                ),
            });
        }
    };
    if output.status.success() {
        return Ok(());
    }
    Err(ScriptFailure {
        message: failure_message(
            output.status.code(),
            exit_signal(&output.status),
            &target.path,
            &shell,
            &args,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
    })
}

fn package_id(manifest: &Value) -> String {
    match (
        manifest.get("name").and_then(Value::as_str),
        manifest.get("version").and_then(Value::as_str),
    ) {
        (Some(name), Some(version)) => format!("{name}@{version}"),
        (Some(name), None) => name.to_string(),
        _ => String::new(),
    }
}

/// Run a root project script with the terminal attached, printing npm's banner first.
pub(super) async fn run_root_script(
    env: &ScriptEnv,
    path: &Path,
    manifest: &Value,
    event: &str,
) -> Result<(), ScriptFailure> {
    let Some(script) = script_for(manifest, path, event) else {
        return Ok(());
    };
    let id = package_id(manifest);
    let mut banner = if manifest.get("name").is_some() && manifest.get("version").is_some() {
        format!("\n> {id} {event}\n")
    } else {
        format!("\n> {event}\n")
    };
    banner.push_str(&format!("> {}\n", script.trim().replace('\n', "\n> ")));
    let taken_over = crate::core::output_guard::is_stdout_taken_over();
    {
        use std::io::Write;
        if taken_over {
            let _ = std::io::stderr().write_all(banner.as_bytes());
        } else {
            let _ = std::io::stdout().write_all(banner.as_bytes());
            let _ = std::io::stdout().flush();
        }
    }
    let (mut command, shell, args) = build_command(env, path, manifest, event, &script, &[]);
    command.stdin(std::process::Stdio::null());
    if taken_over {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            if let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() {
                command.stdout(std::process::Stdio::from(fd));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            if let Ok(handle) = std::io::stderr().as_handle().try_clone_to_owned() {
                command.stdout(std::process::Stdio::from(handle));
            }
        }
    }
    let status = command.status().await.map_err(|error| ScriptFailure {
        message: format!("npm error spawn {shell} {}", error.kind()),
    })?;
    if status.success() {
        return Ok(());
    }
    Err(ScriptFailure {
        message: failure_message(status.code(), exit_signal(&status), path, &shell, &args, "", ""),
    })
}

/// Arborist's `sortNodes`: depth first, then the path compared with `localeCompare('en')`.
pub(super) fn sort_targets(targets: &mut [ScriptTarget]) {
    targets.sort_by(|a, b| {
        a.depth.cmp(&b.depth).then_with(|| {
            pi_tui::collation::locale_compare(
                &a.location.to_string_lossy(),
                &b.location.to_string_lossy(),
            )
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_package_envs_like_run_script() {
        let manifest = serde_json::json!({
            "name": "pkg",
            "version": "1.2.3",
            "config": { "port": "8080", "nested": { "a": true }, "off": false, "list": ["x", "y"] },
            "engines": { "node": ">=18" },
            "bin": { "tool": "cli.js" },
            "scripts": { "test": "x" }
        });
        let mut out = Vec::new();
        package_envs(&manifest, &mut out);
        let get = |k: &str| out.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("npm_package_name"), Some("pkg"));
        assert_eq!(get("npm_package_version"), Some("1.2.3"));
        assert_eq!(get("npm_package_config_port"), Some("8080"));
        assert_eq!(get("npm_package_config_nested_a"), Some("true"));
        assert_eq!(get("npm_package_config_off"), Some(""));
        assert_eq!(get("npm_package_config_list_1"), Some("y"));
        assert_eq!(get("npm_package_engines_node"), Some(">=18"));
        assert_eq!(get("npm_package_bin_tool"), Some("cli.js"));
        assert_eq!(get("npm_package_scripts_test"), None);
    }

    #[test]
    fn gyp_packages_get_the_default_install_script() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binding.gyp"), "{}").unwrap();
        let plain = serde_json::json!({ "name": "native" });
        assert_eq!(
            script_for(&plain, dir.path(), "install").as_deref(),
            Some(DEFAULT_GYP_INSTALL_SCRIPT)
        );
        assert_eq!(script_for(&plain, dir.path(), "postinstall"), None);
        let opted_out = serde_json::json!({ "gypfile": false });
        assert_eq!(script_for(&opted_out, dir.path(), "install"), None);
        let preinstall = serde_json::json!({ "scripts": { "preinstall": "x" } });
        assert_eq!(script_for(&preinstall, dir.path(), "install"), None);
    }

    #[test]
    fn path_lists_every_ancestor_bin_folder_first() {
        let path = script_path(Path::new("/a/b"));
        let parts: Vec<&str> = path.split(if cfg!(windows) { ';' } else { ':' }).collect();
        if cfg!(unix) {
            assert_eq!(&parts[..3], &["/a/b/node_modules/.bin", "/a/node_modules/.bin", "/node_modules/.bin"]);
        }
    }

    #[test]
    fn failure_block_matches_npm() {
        let message = failure_message(
            Some(3),
            None,
            Path::new("/root/node_modules/faildep"),
            "sh",
            &["-c".into(), "echo out-line; echo err-line >&2; exit 3".into()],
            "out-line\n",
            "err-line\n",
        );
        assert_eq!(
            message,
            "npm error code 3\nnpm error path /root/node_modules/faildep\nnpm error command failed\nnpm error command sh -c echo out-line; echo err-line >&2; exit 3\nnpm error out-line\nnpm error err-line"
        );
    }
}
