//! CLI tests that spawn the `pi` binary: ports of `test/session-file-invalid.test.ts`,
//! `test/stdout-cleanliness.test.ts` and `test/startup-session-name.test.ts`. TS spawns
//! `node src/cli.ts`; the fake npm of the stdout test is a shell script instead of a Node script.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_pi(args: &[&str], cwd: &Path, agent_dir: &Path, offline: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .args(args)
        .current_dir(cwd)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if offline {
        command.env("PI_OFFLINE", "1");
    }
    let output = command.output().expect("spawn pi");
    Output {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn temp_root() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let agent_dir = root.join("agent");
    let project_dir = root.join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    (dir, agent_dir, project_dir)
}

#[test]
fn explicit_codemode_tool_survives_no_extensions() {
    let (_dir, agent_dir, project_dir) = temp_root();
    let probe = project_dir.join("probe.js");
    let result_path = project_dir.join("tools.json");
    std::fs::write(&probe, format!(
        "import {{ writeFileSync }} from 'node:fs';\nexport default function(pi) {{ pi.on('session_start', () => writeFileSync({}, JSON.stringify(pi.getActiveTools()))); }}",
        serde_json::to_string(&result_path).unwrap()
    )).unwrap();
    let result = run_pi(
        &[
            "--tools",
            "+codemode",
            "-ne",
            "-e",
            probe.to_str().unwrap(),
            "--mode",
            "rpc",
            "--no-session",
        ],
        &project_dir,
        &agent_dir,
        true,
    );
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    let tools: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(result_path).unwrap()).unwrap();
    assert!(tools.iter().any(|name| name == "codemode"), "{tools:?}");
    assert!(tools.iter().any(|name| name == "read"), "{tools:?}");
}

// session-file-invalid.test.ts
#[test]
fn prints_a_friendly_error_and_preserves_non_session_file_content() {
    let (dir, agent_dir, project_dir) = temp_root();
    let session_file = dir.path().canonicalize().unwrap().join("not-a-session.log");
    let original = "{\"type\":\"event\",\"data\":\"not a session\"}\n";
    std::fs::write(&session_file, original).unwrap();

    let result = run_pi(&["--session", session_file.to_str().unwrap(), "-p", "hi"], &project_dir, &agent_dir, true);

    assert_eq!(result.code, Some(1), "{}", result.stderr);
    assert!(
        result.stderr.contains(&format!("Error: Session file is not a valid pi session: {}", session_file.display())),
        "{}",
        result.stderr
    );
    assert!(!result.stderr.contains("SessionManager.open"));
    assert!(!result.stderr.contains("at "), "{}", result.stderr);
    assert_eq!(std::fs::read_to_string(&session_file).unwrap(), original);
}

// stdout-cleanliness.test.ts
// With stdin at end of file nothing can answer the --resume picker. Node exits once stdin ends and
// its event loop drains (exit code 0, nothing printed after the picker); the port must not hang.
// Like Node, the session list load and the terminal color query (100 ms timeout) still finish
// and are rendered first (the TUI writes to stderr while print mode owns stdout), and the exit
// hook's terminal restore goes to the real stdout (`fs.writeSync(process.stdout.fd)`).
#[test]
fn resume_picker_exits_when_stdin_is_at_eof() {
    let (_dir, agent_dir, project_dir) = temp_root();
    let encoded = format!(
        "--{}--",
        project_dir.to_string_lossy().trim_start_matches('/').replace(['/', ':'], "-")
    );
    let session_dir = agent_dir.join("sessions").join(encoded);
    std::fs::create_dir_all(&session_dir).unwrap();
    let header = serde_json::json!({
        "type": "session", "version": 3, "id": "abc", "timestamp": "2026-01-01T00:00:00.000Z",
        "cwd": project_dir.to_string_lossy(),
    });
    let message = serde_json::json!({
        "type": "message", "id": "m1", "parentId": null, "timestamp": "2026-01-01T00:00:01.000Z",
        "message": { "role": "user", "content": [{ "type": "text", "text": "hello from the resume picker" }],
            "timestamp": 1767225601000i64 }
    });
    std::fs::write(session_dir.join("2026-01-01T00-00-00-000Z_abc.jsonl"), format!("{header}\n{message}\n")).unwrap();

    let start = std::time::Instant::now();
    let result = run_pi(&["--resume", "-p"], &project_dir, &agent_dir, true);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(!result.stdout.contains("No session selected"), "{}", result.stdout);
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
    // TS: `restoreOnExit("\x1b[?25h", "")` with the Kitty keyboard protocol pushed.
    assert_eq!(result.stdout, "\x1b[?25h\x1b[?2004l\x1b[<u");
    let last_frame = &result.stderr[result.stderr.rfind("\x1b[?2026h").expect("a rendered frame")..];
    assert!(last_frame.contains("Resume Session"), "{last_frame:?}");
    assert!(last_frame.contains("hello from the resume picker"), "{last_frame:?}");
    assert!(!last_frame.contains("Loading"), "{last_frame:?}");
}

// Package commands report settings load errors like TS `reportSettingsErrors`: the warning, then
// `chalk.dim(error.stack)`. The stack's first line (`${name}: ${message}`) is reproduced; the V8
// frames that follow in TS (`    at JSON.parse (<anonymous>)`, `    at SettingsManager.loadFromStorage
// (file:///.../settings-manager.js:L:C)`, ...) name the installation's files and are not.
#[test]
fn package_commands_report_settings_errors_with_the_error_stack_header() {
    let (_dir, agent_dir, project_dir) = temp_root();
    std::fs::write(agent_dir.join("settings.json"), "{ invalid json\n").unwrap();
    let result = run_pi(&["list"], &project_dir, &agent_dir, true);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(result.stdout, "No packages installed.\n");
    let message = "Expected property name or '}' in JSON at position 2 (line 1 column 3)";
    assert_eq!(
        result.stderr,
        format!("Warning (package command, global settings): {message}\nSyntaxError: {message}\n")
    );

    // Node fs error wording (`readFileSync` on a directory).
    let (_dir, agent_dir, project_dir) = temp_root();
    std::fs::create_dir_all(agent_dir.join("settings.json")).unwrap();
    let result = run_pi(&["list"], &project_dir, &agent_dir, true);
    let message = "EISDIR: illegal operation on a directory, read";
    assert_eq!(
        result.stderr,
        format!("Warning (package command, global settings): {message}\nError: {message}\n")
    );
}

fn stdout_project() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (dir, agent_dir, project_dir) = temp_root();
    let config_dir = project_dir.join(".pi");
    std::fs::create_dir_all(&config_dir).unwrap();
    let fake_npm = dir.path().join("fake-npm.sh");
    std::fs::write(&fake_npm, "echo \"changed 1 package in 471ms\"\necho \"found 0 vulnerabilities\"\nexit 0\n").unwrap();
    let settings = serde_json::json!({
        "packages": ["npm:fake-package"],
        "npmCommand": ["sh", fake_npm.to_string_lossy()],
    });
    std::fs::write(config_dir.join("settings.json"), serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    (dir, agent_dir, project_dir)
}

#[test]
fn prints_plain_help_to_stdout_when_stdout_is_redirected() {
    let (_dir, agent_dir, project_dir) = stdout_project();
    let result = run_pi(&["--help"], &project_dir, &agent_dir, false);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(result.stdout.contains("Usage:"));
    assert!(!result.stderr.contains("Usage:"));
    assert!(!result.stderr.contains("changed 1 package in 471ms"));
    assert!(!result.stderr.contains("found 0 vulnerabilities"));
}

#[test]
fn keeps_stdout_empty_for_mode_json_help_while_routing_trusted_startup_chatter_to_stderr() {
    let (_dir, agent_dir, project_dir) = stdout_project();
    let result = run_pi(&["--mode", "json", "--help", "--approve"], &project_dir, &agent_dir, false);
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("changed 1 package in 471ms"), "{}", result.stderr);
    assert!(result.stderr.contains("found 0 vulnerabilities"), "{}", result.stderr);
    assert!(result.stderr.contains("Usage:"));
}

// startup-session-name.test.ts
#[test]
fn sets_name_on_the_selected_session_before_runtime_model_validation() {
    let (dir, agent_dir, project_dir) = temp_root();
    let session_file = dir.path().join("session.jsonl");
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let header = serde_json::json!({
        "type": "session", "version": 3, "id": "existing-session", "timestamp": timestamp,
        "cwd": project_dir.to_string_lossy(),
    });
    let message = serde_json::json!({
        "type": "message", "id": "assistant-1", "parentId": null, "timestamp": timestamp,
        "message": {
            "role": "assistant", "content": [{ "type": "text", "text": "hello" }],
            "provider": "anthropic", "model": "claude-sonnet-4-5", "timestamp": chrono::Utc::now().timestamp_millis()
        }
    });
    std::fs::write(&session_file, format!("{header}\n{message}\n")).unwrap();

    let result = run_pi(
        &[
            "--session",
            session_file.to_str().unwrap(),
            "--name",
            "  CLI Named Session  ",
            "--model",
            "missing-model",
            "-p",
            "hi",
        ],
        &project_dir,
        &agent_dir,
        true,
    );

    assert_eq!(result.code, Some(1), "{}", result.stderr);
    let names: Vec<String> = std::fs::read_to_string(&session_file)
        .unwrap()
        .trim()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|entry| entry["type"] == "session_info")
        .map(|entry| entry["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["CLI Named Session"]);
}

// ---------------------------------------------------------------------------
// session-id-readonly.test.ts. In-process cases call `create_session_manager`; TS also spies on
// `console.error` and `SessionManager.list`, which Rust cannot intercept, so those assertions are
// left out. The fork-target case exits the process, so it runs the binary.
// ---------------------------------------------------------------------------

mod session_id {
    use super::*;
    use pi_coding_agent::cli::args::Args;
    use pi_coding_agent::core::session_manager::{NewSessionOptions, SessionManager};
    use pi_coding_agent::core::settings_manager::SettingsManager;
    use pi_coding_agent::pi_main::create_session_manager;
    use serde_json::json;
    use std::sync::Arc;

    fn has_session_with_id(root: &Path, session_id: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else { return false };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && has_session_with_id(&path, session_id) {
                return true;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let first = std::fs::read_to_string(&path).unwrap_or_default().lines().next().unwrap_or_default().to_string();
            if let Ok(header) = serde_json::from_str::<serde_json::Value>(&first)
                && header["type"] == "session"
                && header["id"] == session_id
            {
                return true;
            }
        }
        false
    }

    fn persist_session(session: &SessionManager, content: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        session
            .append_message(pi_agent::types::AgentMessage::from_value(json!({ "role": "user", "content": content, "timestamp": now })).unwrap())
            .unwrap();
        session
            .append_message(
                pi_agent::types::AgentMessage::from_value(json!({
                    "role": "assistant", "content": [{ "type": "text", "text": "persisted" }],
                    "api": "anthropic-messages", "provider": "anthropic", "model": "test",
                    "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
                    "stopReason": "stop", "timestamp": now
                }))
                .unwrap(),
            )
            .unwrap();
    }

    fn settings() -> Arc<SettingsManager> {
        SettingsManager::in_memory(None, None)
    }

    fn create(cwd: &Path, dir: &Path, id: &str) -> Arc<SessionManager> {
        SessionManager::create(
            &cwd.to_string_lossy(),
            Some(&dir.to_string_lossy()),
            Some(&NewSessionOptions { id: Some(id.into()), parent_session: None }),
        )
        .unwrap()
    }

    #[test]
    fn does_not_persist_a_custom_id_for_metadata_commands() {
        let (_dir, agent_dir, project_dir) = temp_root();
        let result = run_pi(&["--session-id", "read-only-help", "--help"], &project_dir, &agent_dir, true);
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        assert!(!has_session_with_id(&agent_dir.join("sessions"), "read-only-help"));
    }

    #[tokio::test]
    async fn creates_missing_ids_and_reopens_existing_ids_in_process() {
        let (dir, _agent_dir, project_dir) = temp_root();
        let session_dir = dir.path().canonicalize().unwrap().join("sessions");
        let cwd = project_dir.to_string_lossy().into_owned();
        let sd = session_dir.to_string_lossy().into_owned();

        let read_only = create_session_manager(
            &Args { session_id: Some("read-only".into()), help: Some(true), ..Default::default() },
            &cwd,
            Some(&sd),
            settings(),
        )
        .await
        .unwrap();
        assert_eq!(read_only.get_session_id(), "read-only");
        assert_eq!(read_only.get_session_file(), None);

        let created = create_session_manager(&Args { session_id: Some("persisted-id".into()), ..Default::default() }, &cwd, Some(&sd), settings())
            .await
            .unwrap();
        persist_session(&created, "persist me");
        let reopened = create_session_manager(&Args { session_id: Some("persisted-id".into()), ..Default::default() }, &cwd, Some(&sd), settings())
            .await
            .unwrap();
        assert_eq!(reopened.get_session_file(), created.get_session_file());
    }

    // Regression test for #9440 (TS also asserts that `SessionManager.list` is not called).
    #[tokio::test]
    async fn looks_up_exact_ids_without_building_full_session_listings() {
        let (dir, _agent_dir, project_dir) = temp_root();
        let session_dir = dir.path().canonicalize().unwrap().join("sessions");
        let unrelated = create(&project_dir, &session_dir, "unrelated-id");
        persist_session(&unrelated, "large transcript contents must not be loaded");
        let created = create_session_manager(
            &Args { session_id: Some("fresh-id".into()), ..Default::default() },
            &project_dir.to_string_lossy(),
            Some(&session_dir.to_string_lossy()),
            settings(),
        )
        .await
        .unwrap();
        assert_eq!(created.get_session_id(), "fresh-id");
    }

    #[tokio::test]
    async fn reopens_an_exact_id_from_a_renamed_session_file() {
        let (dir, _agent_dir, project_dir) = temp_root();
        let session_dir = dir.path().canonicalize().unwrap().join("sessions");
        let original = create(&project_dir, &session_dir, "renamed-id");
        persist_session(&original, "persist me");
        let renamed = session_dir.join("imported-session.jsonl");
        std::fs::rename(original.get_session_file().unwrap(), &renamed).unwrap();
        let reopened = create_session_manager(
            &Args { session_id: Some("renamed-id".into()), ..Default::default() },
            &project_dir.to_string_lossy(),
            Some(&session_dir.to_string_lossy()),
            settings(),
        )
        .await
        .unwrap();
        assert_eq!(reopened.get_session_file(), Some(renamed.to_string_lossy().into_owned()));
    }

    #[test]
    fn filters_exact_ids_by_cwd_in_a_custom_session_directory() {
        let (dir, _agent_dir, _project_dir) = temp_root();
        let root = dir.path().canonicalize().unwrap();
        let (project_a, project_b, session_dir) = (root.join("project-a"), root.join("project-b"), root.join("sessions"));
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();
        let foreign = create(&project_b, &session_dir, "foreign-id");
        persist_session(&foreign, "foreign session");
        let sd = session_dir.to_string_lossy().into_owned();
        assert_eq!(SessionManager::find_by_id(&project_a.to_string_lossy(), "foreign-id", Some(&sd)).unwrap(), None);
        assert_eq!(SessionManager::find_by_id(&project_b.to_string_lossy(), "foreign-id", Some(&sd)).unwrap(), foreign.get_session_file());
    }

    #[test]
    fn rejects_an_existing_fork_target() {
        let (dir, agent_dir, project_dir) = temp_root();
        let session_dir = dir.path().canonicalize().unwrap().join("sessions");
        persist_session(&create(&project_dir, &session_dir, "source-id"), "source");
        persist_session(&create(&project_dir, &session_dir, "existing-id"), "target");
        let result = run_pi(
            &["--session-dir", session_dir.to_str().unwrap(), "--fork", "source-id", "--session-id", "existing-id", "-p", "hi"],
            &project_dir,
            &agent_dir,
            true,
        );
        assert_eq!(result.code, Some(1), "{}", result.stderr);
        assert!(result.stderr.contains("Session already exists with id 'existing-id'"), "{}", result.stderr);
    }
}

// cli/setup.ts sets `process.title = "pi"`; libuv overwrites the command line in place (the
// arguments disappear from `ps`) and sets the thread name.
#[cfg(target_os = "linux")]
#[test]
fn sets_the_process_title_like_node() {
    let (_dir, agent_dir, project_dir) = temp_root();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pi"))
        .args(["--mode", "rpc", "--no-session"])
        .current_dir(&project_dir)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The RPC loop answers once setup has run.
    use std::io::{BufRead, Write};
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{{\"type\":\"get_state\",\"id\":\"1\"}}").unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let proc_dir = format!("/proc/{}", child.id());
    let comm = std::fs::read_to_string(format!("{proc_dir}/comm")).unwrap();
    let cmdline = std::fs::read(format!("{proc_dir}/cmdline")).unwrap();
    drop(stdin);
    let _ = child.wait();
    assert_eq!(comm.trim_end(), "pi");
    let shown: Vec<u8> = cmdline.into_iter().take_while(|b| *b != 0).collect();
    assert_eq!(String::from_utf8(shown).unwrap(), "pi");
}
