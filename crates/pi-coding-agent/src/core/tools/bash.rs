//! Port of `packages/coding-agent/src/core/tools/bash.ts`.

use crate::core::extensions::types::{ExtensionToolContext, ToolDefinition};
use crate::core::tools::js_util::js_number_string;
use crate::core::tools::output_accumulator::{OutputAccumulator, OutputAccumulatorOptions, OutputAccumulatorSnapshotOptions, OutputSnapshot};
use crate::core::tools::renderers::bash::{BASH_UPDATE_THROTTLE_MS, create_shell_renderers};
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::core::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationResult, TruncationResultTruncatedBy, format_size};
use crate::utils::child_process::wait_for_child_process;
use crate::utils::shell::{
    ShellConfig, ShellConfigCommandTransport, get_shell_config, get_shell_env, kill_process_tree,
    track_detached_child_pid, untrack_detached_child_pid,
};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use pi_agent::types::{AgentToolRef, AgentToolResult, AgentToolUpdateCallback};
use pi_ai::types::{ConstrainedSamplingConfig, JsonSchemaStrictness, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// Output limit of `structuredContent.output`, which programmatic callers such as codemode scripts receive.
const STRUCTURED_OUTPUT_MAX_BYTES: i64 = 1024 * 1024;
const MAX_TIMEOUT_SECONDS: f64 = MAX_TIMEOUT_MS / 1000.0;

fn resolve_timeout_ms(timeout: Option<f64>) -> anyhow::Result<Option<f64>> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        anyhow::bail!("Invalid timeout: must be a finite number of seconds");
    }
    let timeout_ms = timeout * 1000.0;
    if timeout_ms > MAX_TIMEOUT_MS {
        anyhow::bail!(
            "Invalid timeout: maximum is {} seconds",
            js_number_string(MAX_TIMEOUT_SECONDS)
        );
    }
    Ok(Some(timeout_ms))
}

pub(crate) const BASH_SNIPPET: &str = "Execute bash commands (ls, grep, find, etc.)";
pub(crate) const BASH_GUIDELINES: &[&str] =
    &["You can inspect PI_* environment variables for current model and session details."];

pub static BASH_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": BASH_SNIPPET, "guidelines": BASH_GUIDELINES }));

/// JSON schema of the bash parameters (TypeBox `bashSchema`).
pub(crate) fn bash_schema() -> Value {
    json!({
        "type": "object",
        "required": ["command"],
        "properties": {
            "command": { "type": "string", "description": "Shell command to execute" },
            "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
        }
    })
}

/// JSON schema of `structuredContent` (TypeBox `bashOutputSchema`).
fn bash_output_schema() -> Value {
    json!({
        "type": "object",
        "required": ["output", "truncated", "exit_code", "wall_time_seconds"],
        "properties": {
            "output": { "type": "string", "description": "Combined stdout and stderr, possibly truncated" },
            "truncated": { "type": "boolean" },
            "full_output_path": { "type": "string", "description": "Full output, when truncated" },
            "exit_code": { "type": "number" },
            "wall_time_seconds": { "type": "number" }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashToolInput {
    /// Shell command to execute
    pub command: String,
    /// Timeout in seconds (optional, no default timeout)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub timeout: Option<f64>,
}

/// Result for programmatic callers such as codemode scripts. A non-zero exit code is an error
/// result for the model, but scripts still resolve to this value. `output` is not limited like the
/// model-facing output: callers decide how much of it reaches the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashToolOutput {
    /// Combined stdout and stderr, possibly truncated
    pub output: String,
    pub truncated: bool,
    /// Full output, when truncated
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub exit_code: i64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub wall_time_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

#[derive(Clone)]
pub struct BashOperationsOptions {
    pub on_data: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    pub signal: Option<CancellationToken>,
    /// Timeout in seconds.
    pub timeout: Option<f64>,
    pub env: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashOperationsResult {
    #[serde(default)]
    pub exit_code: Option<i64>,
}

/// Pluggable operations for the bash tool.
/// Override these to delegate command execution to remote systems (for example SSH).
#[derive(Clone)]
pub struct BashOperations {
    /// Execute a command and stream output.
    /// @param command The command to execute
    /// @param cwd Working directory
    /// @param options Execution options
    /// @returns Promise resolving to the exit code. Report signal terminations as 128 + signal number;
    /// a null exit code is treated as a failed command.
    ///
    /// Errors with the message `aborted` or `timeout:<seconds>` are reported as an aborted or
    /// timed-out command.
    pub exec: Arc<
        dyn Fn(
                String,
                String,
                BashOperationsOptions,
            ) -> BoxFuture<'static, anyhow::Result<BashOperationsResult>>
            + Send
            + Sync,
    >,
}

async fn exec_local_shell(
    shell_name: String,
    resolve_shell_config: Arc<dyn Fn() -> anyhow::Result<ShellConfig> + Send + Sync>,
    command: String,
    cwd: String,
    options: BashOperationsOptions,
) -> anyhow::Result<BashOperationsResult> {
    use crate::core::tools::node_fs;
    let timeout_ms = resolve_timeout_ms(options.timeout)?;
    let signal = options.signal.clone();
    if signal.as_ref().is_some_and(|s| s.is_cancelled()) {
        anyhow::bail!("aborted");
    }
    let shell_config = resolve_shell_config()?;
    if node_fs::access(&cwd, node_fs::F_OK).await.is_err() {
        anyhow::bail!("Working directory does not exist: {cwd}\nCannot execute {shell_name} commands.");
    }

    let command_from_stdin = shell_config.command_transport == Some(ShellConfigCommandTransport::Stdin);
    let mut cmd = tokio::process::Command::new(&shell_config.shell);
    cmd.args(&shell_config.args);
    if !command_from_stdin {
        cmd.arg(&command);
    }
    cmd.current_dir(&cwd);
    cmd.env_clear();
    cmd.envs(options.env.clone().unwrap_or_else(get_shell_env));
    cmd.stdin(if command_from_stdin {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        // `detached: true`: the shell leads a new session and process group, so the whole tree
        // can be killed with one signal.
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|err| {
        let (code, _) = node_fs::io_error_code(&err);
        anyhow::anyhow!("spawn {} {code}", shell_config.shell)
    })?;
    if command_from_stdin && let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(command.as_bytes()).await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    }
    let pid = child.id().map(i64::from);
    if let Some(pid) = pid {
        track_detached_child_pid(pid);
    }

    let on_data = options.on_data.clone();
    let mut forward = move |_stream: crate::utils::child_process::ChildStream, data: &[u8]| on_data(data.to_vec());
    let mut timed_out = false;
    let mut abort_handled = false;
    let result = {
        let wait = wait_for_child_process(&mut child, &mut forward);
        tokio::pin!(wait);
        let timeout_sleep = tokio::time::sleep(Duration::from_millis(timeout_ms.unwrap_or(0.0) as u64));
        tokio::pin!(timeout_sleep);
        let cancelled = async {
            match &signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(cancelled);
        loop {
            tokio::select! {
                result = &mut wait => break result,
                _ = &mut timeout_sleep, if timeout_ms.is_some() && !timed_out => {
                    timed_out = true;
                    if let Some(pid) = pid {
                        kill_process_tree(pid);
                    }
                }
                _ = &mut cancelled, if !abort_handled => {
                    abort_handled = true;
                    if let Some(pid) = pid {
                        kill_process_tree(pid);
                    }
                }
            }
        }
    };
    if let Some(pid) = pid {
        untrack_detached_child_pid(pid);
    }
    let exit = result?;
    if signal.as_ref().is_some_and(|s| s.is_cancelled()) {
        anyhow::bail!("aborted");
    }
    if timed_out {
        anyhow::bail!("timeout:{}", js_number_string(options.timeout.unwrap_or_default()));
    }
    // A signal-killed shell has no exit code. Use the standard shell convention so
    // callers do not mistake the termination for a successful command.
    let exit_code = match (exit.code, exit.signal) {
        (Some(code), _) => code as i64,
        (None, Some(signal)) => 128 + signal as i64,
        (None, None) => 1,
    };
    Ok(BashOperationsResult {
        exit_code: Some(exit_code),
    })
}

/// Shared process execution used by the built-in shell tools.
pub fn create_local_shell_operations(
    shell_name: &str,
    resolve_shell_config: Arc<dyn Fn() -> anyhow::Result<ShellConfig> + Send + Sync>,
) -> BashOperations {
    let shell_name = shell_name.to_string();
    BashOperations {
        exec: Arc::new(move |command, cwd, options| {
            Box::pin(exec_local_shell(
                shell_name.clone(),
                resolve_shell_config.clone(),
                command,
                cwd,
                options,
            ))
        }),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateLocalBashOperationsOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
}

/// Create bash operations using pi's built-in local shell execution backend.
///
/// This is useful for extensions that intercept user_bash and still want pi's
/// standard local shell behavior while wrapping or rewriting commands.
pub fn create_local_bash_operations(
    options: Option<&CreateLocalBashOperationsOptions>,
) -> BashOperations {
    let shell_path = options.and_then(|o| o.shell_path.clone());
    create_local_shell_operations(
        "bash",
        Arc::new(move || get_shell_config(shell_path.as_deref())),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashSpawnContext {
    pub command: String,
    pub cwd: String,
    pub env: IndexMap<String, String>,
}

pub type BashSpawnHook = Arc<dyn Fn(BashSpawnContext) -> BashSpawnContext + Send + Sync>;

fn resolve_spawn_context(
    command: String,
    cwd: String,
    spawn_hook: Option<&BashSpawnHook>,
    expose_session_environment: bool,
    ctx: Option<&ExtensionToolContext>,
) -> BashSpawnContext {
    let mut env = get_shell_env();
    for key in ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_MODEL", "PI_REASONING_LEVEL"] {
        env.shift_remove(key);
    }
    if expose_session_environment && let Some(ctx) = ctx {
        if let Ok(session_manager) = ctx.session_manager() {
            env.insert("PI_SESSION_ID".to_string(), session_manager.get_session_id());
            if let Some(session_file) = session_manager.get_session_file()
                && !session_file.is_empty()
            {
                env.insert("PI_SESSION_FILE".to_string(), session_file);
            }
        }
        if let Ok(Some(model)) = ctx.model() {
            env.insert("PI_PROVIDER".to_string(), model.provider.clone());
            env.insert("PI_MODEL".to_string(), model.id.clone());
        }
        if let Ok(Some(level)) = ctx.thinking_level() {
            env.insert("PI_REASONING_LEVEL".to_string(), level.as_str().to_string());
        }
    }
    let base = BashSpawnContext { command, cwd, env };
    match spawn_hook {
        Some(hook) => hook(base),
        None => base,
    }
}

#[derive(Clone, Default)]
pub struct BashToolOptions {
    /// Custom operations for command execution. Default: local shell
    pub operations: Option<BashOperations>,
    /// Command prefix prepended to every command (for example shell setup commands)
    pub command_prefix: Option<String>,
    /// Optional explicit shell path from settings
    pub shell_path: Option<String>,
    /// Expose current Pi session metadata as PI_* environment variables. Default: true
    pub expose_session_environment: Option<bool>,
    /// Hook to adjust command, cwd, or env before execution
    pub spawn_hook: Option<BashSpawnHook>,
}

/// Renderer state of the shell tools (`startedAt`, `endedAt`, `interval`). The renderers keep it
/// as [`crate::core::tools::render_utils::ToolTimerState`] in `ToolRenderContext.state`.
pub type BashRenderState = crate::core::tools::render_utils::ToolTimerState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellToolConfig {
    pub name: String,
    pub label: String,
    pub shell_name: String,
    pub prompt: String,
    pub prompt_snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_guidelines: Option<Vec<String>>,
    pub temp_file_prefix: String,
}

/// The working directory of a tool call: `ctx?.cwd || cwd`.
pub(crate) fn effective_cwd(ctx: Option<&ExtensionToolContext>, cwd: &str) -> String {
    ctx.and_then(|ctx| ctx.cwd().ok())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Parse tool parameters into their input type, reporting schema mismatches as errors.
pub(crate) fn parse_tool_input<T: serde::de::DeserializeOwned>(tool: &str, params: Value) -> anyhow::Result<T> {
    serde_json::from_value(params).map_err(|err| anyhow::anyhow!("Invalid arguments for {tool}: {err}"))
}

struct UpdateState {
    dirty: bool,
    last_update_at: Option<Instant>,
    timer: Option<tokio::task::JoinHandle<()>>,
}

struct ShellRun {
    output: OutputAccumulator,
    accepting_output: AtomicBool,
    on_update: Option<AgentToolUpdateCallback>,
    updates: parking_lot::Mutex<UpdateState>,
}

impl ShellRun {
    fn emit_output_update(&self) {
        let Some(on_update) = &self.on_update else {
            return;
        };
        {
            let mut updates = self.updates.lock();
            if !updates.dirty {
                return;
            }
            updates.dirty = false;
            updates.last_update_at = Some(Instant::now());
        }
        let snapshot = self.output.snapshot(Some(&OutputAccumulatorSnapshotOptions {
            persist_if_truncated: Some(true),
        }));
        let details = BashToolDetails {
            truncation: snapshot.truncation.truncated.then(|| snapshot.truncation.clone()),
            full_output_path: snapshot.full_output_path.clone(),
        };
        on_update(AgentToolResult {
            content: vec![UserContentBlock::text(snapshot.content)],
            details: Some(serde_json::to_value(details).unwrap_or(Value::Null)),
            ..Default::default()
        });
    }

    fn clear_update_timer(&self) {
        if let Some(timer) = self.updates.lock().timer.take() {
            timer.abort();
        }
    }

    fn schedule_output_update(self: &Arc<Self>) {
        if self.on_update.is_none() {
            return;
        }
        let delay = {
            let mut updates = self.updates.lock();
            updates.dirty = true;
            let elapsed = updates
                .last_update_at
                .map(|t| t.elapsed().as_millis() as i64)
                .unwrap_or(i64::MAX / 2);
            BASH_UPDATE_THROTTLE_MS - elapsed
        };
        if delay <= 0 {
            self.clear_update_timer();
            self.emit_output_update();
            return;
        }
        let mut updates = self.updates.lock();
        if updates.timer.is_none() {
            let run = Arc::downgrade(self);
            updates.timer = Some(crate::utils::uncaught::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                if let Some(run) = run.upgrade() {
                    run.updates.lock().timer = None;
                    run.emit_output_update();
                }
            }));
        }
    }

    fn handle_data(self: &Arc<Self>, data: &[u8]) {
        if !self.accepting_output.load(Ordering::SeqCst) {
            return;
        }
        if self.output.append(data).is_ok() {
            self.schedule_output_update();
        }
    }

    async fn finish_output(&self) -> anyhow::Result<OutputSnapshot> {
        self.accepting_output.store(false, Ordering::SeqCst);
        self.output.finish();
        self.clear_update_timer();
        self.emit_output_update();
        let snapshot = self.output.snapshot(Some(&OutputAccumulatorSnapshotOptions {
            persist_if_truncated: Some(true),
        }));
        self.output.close_temp_file().await?;
        Ok(snapshot)
    }

    fn format_output(&self, snapshot: &OutputSnapshot, empty_text: &str) -> (String, Option<BashToolDetails>) {
        let truncation = &snapshot.truncation;
        let mut text = if snapshot.content.is_empty() {
            empty_text.to_string()
        } else {
            snapshot.content.clone()
        };
        let mut details = None;
        if truncation.truncated {
            details = Some(BashToolDetails {
                truncation: Some(truncation.clone()),
                full_output_path: snapshot.full_output_path.clone(),
            });
            let start_line = truncation.total_lines - truncation.output_lines + 1;
            let end_line = truncation.total_lines;
            let full_output_path = snapshot.full_output_path.clone().unwrap_or_else(|| "undefined".to_string());
            if truncation.last_line_partial {
                let last_line_size = format_size(self.output.get_last_line_bytes());
                text.push_str(&format!(
                    "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {full_output_path}]",
                    format_size(truncation.output_bytes)
                ));
            } else if truncation.truncated_by == Some(TruncationResultTruncatedBy::Lines) {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {full_output_path}]",
                    truncation.total_lines
                ));
            } else {
                text.push_str(&format!(
                    "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_output_path}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                ));
            }
        }
        (text, details)
    }
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_shell_tool(
    ops: BashOperations,
    cwd: String,
    temp_file_prefix: String,
    command_prefix: Option<String>,
    expose_session_environment: bool,
    spawn_hook: Option<BashSpawnHook>,
    params: Value,
    signal: Option<CancellationToken>,
    on_update: Option<AgentToolUpdateCallback>,
    ctx: Option<ExtensionToolContext>,
    tool_name: String,
) -> anyhow::Result<AgentToolResult> {
    let input: BashToolInput = parse_tool_input(&tool_name, params)?;
    let resolved_command = match command_prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(prefix) => format!("{prefix}\n{}", input.command),
        None => input.command.clone(),
    };
    let spawn_context = resolve_spawn_context(
        resolved_command,
        effective_cwd(ctx.as_ref(), &cwd),
        spawn_hook.as_ref(),
        expose_session_environment,
        ctx.as_ref(),
    );

    let run = Arc::new(ShellRun {
        output: OutputAccumulator::new(Some(&OutputAccumulatorOptions {
            max_lines: None,
            max_bytes: None,
            temp_file_prefix: Some(temp_file_prefix),
        })),
        accepting_output: AtomicBool::new(true),
        on_update: on_update.clone(),
        updates: parking_lot::Mutex::new(UpdateState {
            dirty: false,
            last_update_at: None,
            timer: None,
        }),
    });

    if let Some(on_update) = &on_update {
        on_update(AgentToolResult {
            content: Vec::new(),
            details: None,
            ..Default::default()
        });
    }

    let started_at = Instant::now();
    let data_run = run.clone();
    let exec_result = (ops.exec)(
        spawn_context.command,
        spawn_context.cwd,
        BashOperationsOptions {
            on_data: Arc::new(move |data: Vec<u8>| data_run.handle_data(&data)),
            signal,
            timeout: input.timeout,
            env: Some(spawn_context.env),
        },
    )
    .await;

    let result = async {
        let exit_code = match exec_result {
            Ok(result) => result.exit_code,
            Err(err) => {
                let snapshot = run.finish_output().await?;
                let (text, _) = run.format_output(&snapshot, "");
                let message = err.to_string();
                if message == "aborted" {
                    anyhow::bail!(append_status(&text, "Command aborted"));
                }
                if let Some(timeout_secs) = message.strip_prefix("timeout:") {
                    let timeout_secs = timeout_secs.split(':').next().unwrap_or_default();
                    anyhow::bail!(append_status(
                        &text,
                        &format!("Command timed out after {timeout_secs} seconds")
                    ));
                }
                return Err(err);
            }
        };

        let snapshot = run.finish_output().await?;
        let (output_text, details) = run.format_output(&snapshot, "(no output)");
        let Some(exit_code) = exit_code else {
            anyhow::bail!(append_status(&output_text, "Command terminated without an exit code"));
        };
        let wall_time_seconds = (started_at.elapsed().as_secs_f64() * 1000.0 / 100.0).round() / 10.0;
        let full_output = run.output.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES).await?;
        let structured = BashToolOutput {
            output: full_output.content,
            truncated: full_output.truncated,
            full_output_path: if full_output.truncated {
                snapshot.full_output_path.clone()
            } else {
                None
            },
            exit_code,
            wall_time_seconds,
        };
        let details = details.map(|d| serde_json::to_value(d).unwrap_or(Value::Null));
        let structured_content = Some(serde_json::to_value(structured)?);
        if exit_code != 0 {
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(append_status(
                    &output_text,
                    &format!("Command exited with code {exit_code}"),
                ))],
                details,
                structured_content,
                is_error: Some(true),
                ..Default::default()
            });
        }
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(output_text)],
            details,
            structured_content,
            ..Default::default()
        })
    }
    .await;
    run.clear_update_timer();
    result
}

pub fn create_shell_tool_definition(
    cwd: &str,
    config: &ShellToolConfig,
    options: Option<&BashToolOptions>,
) -> ToolDefinition {
    let ops = options
        .and_then(|o| o.operations.clone())
        .unwrap_or_else(|| {
            create_local_bash_operations(Some(&CreateLocalBashOperationsOptions {
                shell_path: options.and_then(|o| o.shell_path.clone()),
            }))
        });
    let command_prefix = options.and_then(|o| o.command_prefix.clone());
    let expose_session_environment = options
        .and_then(|o| o.expose_session_environment)
        .unwrap_or(true);
    let spawn_hook = options.and_then(|o| o.spawn_hook.clone());
    let cwd = cwd.to_string();
    let temp_file_prefix = config.temp_file_prefix.clone();
    let tool_name = config.name.clone();

    let mut definition = ToolDefinition::new(
        config.name.clone(),
        config.label.clone(),
        format!(
            "Execute a {} command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            config.shell_name,
            DEFAULT_MAX_BYTES / 1024
        ),
        bash_schema(),
        move |_tool_call_id, params, signal, on_update, ctx| {
            execute_shell_tool(
                ops.clone(),
                cwd.clone(),
                temp_file_prefix.clone(),
                command_prefix.clone(),
                expose_session_environment,
                spawn_hook.clone(),
                params,
                signal,
                on_update,
                ctx,
                tool_name.clone(),
            )
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(config.prompt_snippet.clone());
    definition.prompt_guidelines = if expose_session_environment {
        config.prompt_guidelines.clone()
    } else {
        None
    };
    definition.output_schema = Some(bash_output_schema());
    definition.constrained_sampling = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: JsonSchemaStrictness::Prefer,
    });
    let renderers = create_shell_renderers(&config.prompt);
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

fn bash_tool_config() -> ShellToolConfig {
    ShellToolConfig {
        name: "bash".to_string(),
        label: "bash".to_string(),
        shell_name: "bash".to_string(),
        prompt: "$".to_string(),
        prompt_snippet: BASH_SNIPPET.to_string(),
        prompt_guidelines: Some(BASH_GUIDELINES.iter().map(|s| s.to_string()).collect()),
        temp_file_prefix: "pi-bash".to_string(),
    }
}

pub fn create_bash_tool_definition(cwd: &str, options: Option<&BashToolOptions>) -> ToolDefinition {
    create_shell_tool_definition(cwd, &bash_tool_config(), options)
}

/// The TS version also copies `promptSnippet`/`promptGuidelines` onto the returned tool; Rust
/// `AgentTool`s carry no prompt metadata, so use [`create_bash_tool_definition`] for those.
pub fn create_bash_tool(cwd: &str, options: Option<&BashToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_bash_tool_definition(cwd, options), None)
}
