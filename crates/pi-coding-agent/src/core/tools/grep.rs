//! Port of `packages/coding-agent/src/core/tools/grep.ts`.

use crate::core::extensions::types::ToolDefinition;
use crate::core::tools::bash::{effective_cwd, parse_tool_input};
use crate::core::tools::js_util::js_number_string;
use crate::core::tools::path_utils::{node_path_relative, resolve_to_cwd};
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::core::tools::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, MAX_SAFE_INTEGER, TruncationOptions, TruncationResult, format_size,
    truncate_head, truncate_line,
};
use crate::utils::tools_manager::{GetToolPathTool, ensure_tool};
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::types::UserContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use std::sync::Arc;
use std::sync::LazyLock;

const GREP_SNIPPET: &str = "Search file contents for patterns (respects .gitignore)";

pub static GREP_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": GREP_SNIPPET, "guidelines": [] }));

const DEFAULT_LIMIT: f64 = 100.0;

fn grep_schema() -> Value {
    json!({
        "type": "object",
        "required": ["pattern"],
        "properties": {
            "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
            "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
            "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
            "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
            "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
            "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
            "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepToolInput {
    /// Search pattern (regex or literal string)
    pub pattern: String,
    /// Directory or file to search (default: current directory)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Case-insensitive search (default: false)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_case: Option<bool>,
    /// Treat pattern as literal string instead of regex (default: false)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<bool>,
    /// Number of lines to show before and after each match (default: 0)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub context: Option<f64>,
    /// Maximum number of matches to return (default: 100)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub match_limit_reached: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_truncated: Option<bool>,
}

/// Pluggable operations for the grep tool.
/// Override these to delegate search to remote systems (for example SSH).
#[derive(Clone)]
pub struct GrepOperations {
    /// Check if path is a directory. Throws if path does not exist.
    pub is_directory: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>,
    /// Read file contents for context lines
    pub read_file: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>,
}

#[derive(Clone, Default)]
pub struct GrepToolOptions {
    /// Custom operations for grep. Default: local filesystem plus ripgrep
    pub operations: Option<GrepOperations>,
}

fn default_grep_operations() -> GrepOperations {
    use crate::core::tools::node_fs;
    GrepOperations {
        is_directory: Arc::new(|path| Box::pin(async move { Ok(node_fs::stat(&path).await?.is_dir()) })),
        read_file: Arc::new(|path| {
            Box::pin(async move {
                let bytes = node_fs::read_file(&path).await?;
                Ok(crate::core::tools::output_accumulator::decode_utf8_lossy(&bytes))
            })
        }),
    }
}

/// `child.kill()`: SIGTERM on Unix.
pub(crate) fn terminate_child(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id()
        && let Ok(pid) = libc::pid_t::try_from(pid)
    {
        // SAFETY: kill(2) on our own child process.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.id();
    }
}

struct GrepMatch {
    file_path: String,
    line_number: i64,
    line_text: Option<String>,
}

struct GrepFormatter<'a> {
    ops: &'a GrepOperations,
    search_path: String,
    is_directory: bool,
    context_value: f64,
    file_cache: HashMap<String, Vec<String>>,
    lines_truncated: bool,
}

impl GrepFormatter<'_> {
    fn format_path(&self, file_path: &str) -> String {
        if self.is_directory {
            let relative = node_path_relative(&self.search_path, file_path);
            if !relative.is_empty() && !relative.starts_with("..") {
                return relative.replace('\\', "/");
            }
        }
        std::path::Path::new(file_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    async fn get_file_lines(&mut self, file_path: &str) -> Vec<String> {
        if let Some(lines) = self.file_cache.get(file_path) {
            return lines.clone();
        }
        let lines = match (self.ops.read_file)(file_path.to_string()).await {
            Ok(content) => content
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .split('\n')
                .map(str::to_string)
                .collect(),
            Err(_) => Vec::new(),
        };
        self.file_cache.insert(file_path.to_string(), lines.clone());
        lines
    }

    async fn format_block(&mut self, file_path: &str, line_number: i64) -> Vec<String> {
        let relative_path = self.format_path(file_path);
        let lines = self.get_file_lines(file_path).await;
        if lines.is_empty() {
            return vec![format!("{relative_path}:{line_number}: (unable to read file)")];
        }
        let mut block = Vec::new();
        let context = self.context_value;
        let (start, end) = if context > 0.0 {
            (
                ((line_number as f64 - context).max(1.0)).ceil() as i64,
                ((line_number as f64 + context).min(lines.len() as f64)).floor() as i64,
            )
        } else {
            (line_number, line_number)
        };
        let mut current = start;
        while current <= end {
            let line_text = lines.get((current - 1) as usize).cloned().unwrap_or_default();
            let sanitized = line_text.replace('\r', "");
            let truncated = truncate_line(&sanitized, None);
            if truncated.was_truncated {
                self.lines_truncated = true;
            }
            if current == line_number {
                block.push(format!("{relative_path}:{current}: {}", truncated.text));
            } else {
                block.push(format!("{relative_path}-{current}- {}", truncated.text));
            }
            current += 1;
        }
        block
    }
}

async fn execute_grep(
    custom_ops: Option<GrepOperations>,
    cwd: String,
    input: GrepToolInput,
    signal: CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    let Some(rg_path) = ensure_tool(GetToolPathTool::Rg, None).await? else {
        anyhow::bail!("ripgrep (rg) is not available and could not be downloaded");
    };

    let search_path = resolve_to_cwd(input.path.as_deref().filter(|p| !p.is_empty()).unwrap_or("."), &cwd);
    let ops = custom_ops.unwrap_or_else(default_grep_operations);
    let is_directory = match (ops.is_directory)(search_path.clone()).await {
        Ok(is_directory) => is_directory,
        Err(_) => anyhow::bail!("Path not found: {search_path}"),
    };

    let context_value = match input.context {
        Some(context) if context > 0.0 => context,
        _ => 0.0,
    };
    let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1.0);

    // Keep the tool contract independent of ambient RIPGREP_CONFIG_PATH settings.
    let mut args: Vec<String> = vec![
        "--no-config".into(),
        "--json".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--hidden".into(),
    ];
    if input.ignore_case.unwrap_or(false) {
        args.push("--ignore-case".into());
    }
    if input.literal.unwrap_or(false) {
        args.push("--fixed-strings".into());
    }
    if let Some(glob) = input.glob.as_deref().filter(|g| !g.is_empty()) {
        args.push("--glob".into());
        args.push(glob.to_string());
    }
    args.push("--".into());
    args.push(input.pattern.clone());
    args.push(search_path.clone());

    let mut child = match tokio::process::Command::new(&rg_path)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let (code, _) = crate::core::tools::node_fs::io_error_code(&err);
            anyhow::bail!("Failed to run ripgrep: spawn {rg_path} {code}");
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut match_count = 0f64;
    let mut match_limit_reached = false;
    let mut killed_due_to_limit = false;
    let mut aborted = false;
    let mut killed = false;
    let mut matches: Vec<GrepMatch> = Vec::new();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    _ => break,
                };
                if line.trim().is_empty() || match_count >= effective_limit {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if event.get("type").and_then(Value::as_str) == Some("match") {
                    match_count += 1.0;
                    let data = event.get("data");
                    let file_path = data
                        .and_then(|d| d.get("path"))
                        .and_then(|p| p.get("text"))
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty());
                    let line_number = data.and_then(|d| d.get("line_number")).and_then(Value::as_i64);
                    let line_text = data
                        .and_then(|d| d.get("lines"))
                        .and_then(|l| l.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let (Some(file_path), Some(line_number)) = (file_path, line_number) {
                        matches.push(GrepMatch {
                            file_path: file_path.to_string(),
                            line_number,
                            line_text,
                        });
                    }
                    if match_count >= effective_limit {
                        match_limit_reached = true;
                        if !killed {
                            killed = true;
                            killed_due_to_limit = true;
                            terminate_child(&child);
                        }
                    }
                }
            }
            _ = signal.cancelled(), if !aborted => {
                aborted = true;
                if !killed {
                    killed = true;
                    terminate_child(&child);
                }
            }
        }
    }

    let status = child.wait().await;
    let stderr = stderr_task.await.unwrap_or_default();
    if aborted || signal.is_cancelled() {
        anyhow::bail!("Operation aborted");
    }
    let code = match status {
        Ok(status) => status.code(),
        Err(err) => anyhow::bail!("Failed to run ripgrep: {err}"),
    };
    if !killed_due_to_limit && code != Some(0) && code != Some(1) {
        let trimmed = stderr.trim();
        let error_msg = if trimmed.is_empty() {
            format!(
                "ripgrep exited with code {}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string())
            )
        } else {
            trimmed.to_string()
        };
        anyhow::bail!(error_msg);
    }
    if match_count == 0.0 {
        return Ok(AgentToolResult {
            content: vec![UserContentBlock::text("No matches found")],
            details: None,
            ..Default::default()
        });
    }

    // Format matches after streaming finishes so custom readFile() backends can be async.
    let mut formatter = GrepFormatter {
        ops: &ops,
        search_path: search_path.clone(),
        is_directory,
        context_value,
        file_cache: HashMap::new(),
        lines_truncated: false,
    };
    let mut output_lines: Vec<String> = Vec::new();
    for m in &matches {
        if context_value == 0.0
            && let Some(line_text) = &m.line_text
        {
            let relative_path = formatter.format_path(&m.file_path);
            let sanitized = line_text.replace("\r\n", "\n").replace('\r', "");
            let sanitized = sanitized.strip_suffix('\n').unwrap_or(&sanitized);
            let truncated = truncate_line(sanitized, None);
            if truncated.was_truncated {
                formatter.lines_truncated = true;
            }
            output_lines.push(format!("{relative_path}:{}: {}", m.line_number, truncated.text));
        } else {
            let block = formatter.format_block(&m.file_path, m.line_number).await;
            output_lines.extend(block);
        }
    }

    let raw_output = output_lines.join("\n");
    // Apply byte truncation. There is no line limit here because the match limit already capped rows.
    let truncation = truncate_head(
        &raw_output,
        Some(&TruncationOptions {
            max_lines: Some(MAX_SAFE_INTEGER),
            max_bytes: None,
        }),
    );
    let mut output = truncation.content.clone();
    let mut details = GrepToolDetails::default();
    let mut notices: Vec<String> = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{} matches limit reached. Use limit={} for more, or refine pattern",
            js_number_string(effective_limit),
            js_number_string(effective_limit * 2.0)
        ));
        details.match_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if formatter.lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.lines_truncated = Some(true);
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let has_details =
        details.match_limit_reached.is_some() || details.truncation.is_some() || details.lines_truncated.is_some();
    Ok(AgentToolResult {
        content: vec![UserContentBlock::text(output)],
        details: if has_details { Some(serde_json::to_value(details)?) } else { None },
        ..Default::default()
    })
}

pub fn create_grep_tool_definition(cwd: &str, options: Option<&GrepToolOptions>) -> ToolDefinition {
    let custom_ops = options.and_then(|o| o.operations.clone());
    let cwd = cwd.to_string();
    let mut definition = ToolDefinition::new(
        "grep",
        "grep",
        format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
            js_number_string(DEFAULT_LIMIT),
            DEFAULT_MAX_BYTES / 1024
        ),
        grep_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let custom_ops = custom_ops.clone();
            let cwd = cwd.clone();
            async move {
                let input: GrepToolInput = parse_tool_input("grep", params)?;
                let signal = signal.unwrap_or_default();
                if signal.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                execute_grep(custom_ops, effective_cwd(ctx.as_ref(), &cwd), input, signal).await
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(GREP_SNIPPET.to_string());
    let renderers = crate::core::tools::renderers::grep::grep_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_grep_tool(cwd: &str, options: Option<&GrepToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_grep_tool_definition(cwd, options), None)
}
