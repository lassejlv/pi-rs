use super::{api::CodingToolApi, require_env};
use crate::{
    env::*,
    harness::{
        declaration::ToolDeclaration,
        live::{DiagnosticSeverity, ToolDiagnostic},
        tool::{NativeTool, ToolExecutionResult},
    },
    storage::{StorageFuture, line::eager},
    truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
    *,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BashToolInput {
    pub command: JsonString,
    pub timeout: Option<f64>,
}
pub type PowerShellToolInput = BashToolInput;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecution {
    pub command: JsonString,
    pub cwd: PathBuf,
    pub env: JsonObject,
    pub inherit_env: bool,
}
pub type BashPrepare = Arc<
    dyn Fn(BashExecution, Arc<dyn CodingToolApi>, Context) -> StorageFuture<BashExecution>
        + Send
        + Sync,
>;
#[derive(Clone, Default)]
pub struct BashToolOptions {
    pub command_prefix: Option<JsonString>,
    pub prepare: Option<BashPrepare>,
}
#[derive(Clone, Default)]
pub struct PowerShellToolOptions {
    pub command_prefix: Option<JsonString>,
    pub prepare: Option<BashPrepare>,
    pub programs: Option<Vec<JsonString>>,
}
fn validate_timeout(timeout: Option<f64>) -> Result<()> {
    if let Some(timeout) = timeout {
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err(Error::message(
                "Invalid timeout: must be a finite number of seconds",
            ));
        }
        if timeout > 2_147_483.647 {
            return Err(Error::message(
                "Invalid timeout: maximum is 2147483.647 seconds",
            ));
        }
    }
    Ok(())
}
pub fn execute_shell(
    args: BashToolInput,
    options: BashToolOptions,
    programs: Option<Vec<JsonString>>,
    api: Arc<dyn CodingToolApi>,
    context: Context,
) -> StorageFuture<ToolExecutionResult> {
    eager(async move {
        validate_timeout(args.timeout)?;
        let env = require_env(api.environment()?)?;
        let mut command = args.command;
        if let Some(mut prefix) = options.command_prefix.filter(|p| !p.units().is_empty()) {
            prefix.append(&"\n".into());
            prefix.append(&command);
            command = prefix;
        }
        let mut execution = BashExecution {
            command,
            cwd: env.cwd(),
            env: JsonObject::default(),
            inherit_env: true,
        };
        if let Some(prepare) = options.prepare {
            execution = prepare(execution, api.clone(), context.clone()).await?;
        }
        let commands = if let Some(programs) = programs {
            let mut script = JsonString::from(
                "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n",
            );
            script.append(&execution.command);
            programs
                .into_iter()
                .map(|program| {
                    let mut args = vec![program];
                    args.extend(
                        [
                            "-NoProfile",
                            "-NonInteractive",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-Command",
                        ]
                        .map(JsonString::from),
                    );
                    args.push(script.clone());
                    ShellCommand::Argv(args)
                })
                .collect::<Vec<_>>()
        } else {
            vec![ShellCommand::Script(execution.command)]
        };
        let env = require_env(api.environment()?)?;
        let mut result = None;
        for command in commands {
            let output = api.clone();
            let options = ShellExecOptions {
                cwd: Some(execution.cwd.clone()),
                env: Some(execution.env.clone()),
                inherit_env: Some(execution.inherit_env),
                timeout: args.timeout,
                spill: Some(ShellSpillOptions {
                    after_bytes: DEFAULT_MAX_BYTES as f64,
                    after_lines: DEFAULT_MAX_LINES as f64,
                }),
                window: api.output_window()?,
                on_output: Some(Arc::new(move |text, _, info| {
                    output.output(text, info.skipped).map_err(|error| {
                        ExecutionError::new(ExecutionErrorCode::CallbackError, error.to_string())
                            .with_cause(Arc::new(error))
                    })
                })),
            };
            let attempt = env.exec(command, options, &context).await;
            let retry = attempt
                .as_ref()
                .is_err_and(|error| error.code() == ExecutionErrorCode::SpawnError);
            result = Some(attempt);
            if !retry {
                break;
            }
        }
        let result = result.ok_or_else(|| Error::message("No command to run"))?;
        let spill = result
            .as_ref()
            .ok()
            .and_then(|v| v.spill_path.clone())
            .or_else(|| {
                result
                    .as_ref()
                    .err()
                    .and_then(|e| e.spill_path().map(Into::into))
            });
        if let Some(path) = &spill {
            api.diagnostic(ToolDiagnostic {
                severity: DiagnosticSeverity::Info,
                code: Some("full_output".into()),
                message: format!("Full output: {}", path.display()).into(),
            })?;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return Err(match error.code() {
                    ExecutionErrorCode::Aborted if context.check().is_err() => {
                        Error::execution(error)
                    }
                    ExecutionErrorCode::Timeout => Error::message(format!(
                        "Command timed out after {} seconds",
                        args.timeout
                            .map(pi_json::number_to_string)
                            .unwrap_or_else(|| "undefined".into())
                    )),
                    ExecutionErrorCode::Aborted => Error::message("Command aborted"),
                    _ => Error::execution(error),
                });
            }
        };
        let retained = api.retained_output()?;
        let mut structured = JsonObject::default();
        structured.insert("output".into(), wire::encode(&retained.text));
        structured.insert("truncated".into(), wire::encode(&retained.truncated));
        structured.insert("exitCode".into(), wire::encode(&result.exit_code));
        if let Some(path) = spill {
            structured.insert("fullOutputPath".into(), wire::encode(&path));
        }
        Ok(ToolExecutionResult {
            structured_output: Some(structured.into_value().into()),
            is_error: (result.exit_code != 0).then_some(true),
            diagnostics: if result.exit_code == 0 {
                None
            } else {
                Some(vec![ToolDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    code: Some("exit_code".into()),
                    message: format!("Command exited with code {}", result.exit_code).into(),
                }])
            },
            ..Default::default()
        })
    })
}
fn shell_tool(
    name: &str,
    options: BashToolOptions,
    programs: Option<Vec<JsonString>>,
) -> NativeTool {
    let title = if name == "bash" { "bash" } else { "PowerShell" };
    let parameters = wire::encode(
        &serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":format!("{title} command to execute")},"timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}},"required":["command"]}),
    );
    let structured = wire::encode(
        &serde_json::json!({"type":"object","properties":{"output":{"type":"string","description":"Combined stdout and stderr: the retained tail, as the model sees it"},"truncated":{"type":"boolean","description":"Whether earlier output was dropped"},"fullOutputPath":{"type":"string","description":"File with the complete output, when it was spilled"},"exitCode":{"type":"number"}},"required":["output","truncated","exitCode"]}),
    );
    let mut tool = NativeTool::new(
        ToolDeclaration::new(
            name,
            format!(
                "Execute a {title} command in the current working directory. Returns combined stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters,
        ),
        Arc::new(move |args, api, context| match wire::decode(args.value()) {
            Ok(args) => execute_shell(
                args,
                options.clone(),
                programs.clone(),
                Arc::new(api),
                context,
            ),
            Err(error) => Box::pin(async { Err(Error::message(error)) }),
        }),
    );
    tool.structured_output_schema = Some(structured);
    tool.output_limits.retain = pi_chord::output::Retain::Tail;
    tool
}
pub fn create_bash_tool(options: BashToolOptions) -> NativeTool {
    shell_tool("bash", options, None)
}
pub fn create_powershell_tool(options: PowerShellToolOptions) -> NativeTool {
    shell_tool(
        "powershell",
        BashToolOptions {
            command_prefix: options.command_prefix,
            prepare: options.prepare,
        },
        Some(
            options
                .programs
                .unwrap_or_else(|| vec!["pwsh".into(), "powershell".into()]),
        ),
    )
}
