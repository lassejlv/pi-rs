//! Port of `packages/coding-agent/src/core/tools/find.ts`.

use crate::core::extensions::types::ToolDefinition;
use crate::core::tools::bash::{effective_cwd, parse_tool_input};
use crate::core::tools::grep::terminate_child;
use crate::core::tools::js_util::js_number_string;
use crate::core::tools::path_utils::{path_exists, resolve_to_cwd};
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::core::tools::truncate::{
    DEFAULT_MAX_BYTES, MAX_SAFE_INTEGER, TruncationOptions, TruncationResult, format_size, truncate_head,
};
use crate::utils::tools_manager::{GetToolPathTool, ensure_tool};
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::types::UserContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use std::sync::Arc;
use std::sync::LazyLock;

/// Which `path` module semantics to use (TS passes `path.posix` or `path.win32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFlavor {
    Posix,
    Win32,
}

impl PathFlavor {
    /// The flavor of the current platform (`path`).
    pub fn native() -> Self {
        if cfg!(windows) { Self::Win32 } else { Self::Posix }
    }

    fn sep(self) -> char {
        match self {
            Self::Posix => '/',
            Self::Win32 => '\\',
        }
    }

    fn is_absolute(self, path: &str) -> bool {
        match self {
            Self::Posix => path.starts_with('/'),
            Self::Win32 => {
                let bytes = path.as_bytes();
                matches!(bytes.first(), Some(b'\\') | Some(b'/'))
                    || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/'))
            }
        }
    }

    /// Split an absolute path into its root (drive or `/`) and normalized segments.
    fn split_absolute(self, path: &str) -> (String, Vec<String>) {
        let (root, rest) = match self {
            Self::Posix => ("/".to_string(), path.to_string()),
            Self::Win32 => {
                let normalized = path.replace('/', "\\");
                let bytes = normalized.as_bytes();
                if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                    (normalized[..2].to_ascii_uppercase(), normalized[2..].to_string())
                } else {
                    (String::new(), normalized)
                }
            }
        };
        let mut segments: Vec<String> = Vec::new();
        for part in rest.split(|c| c == '/' || (self == Self::Win32 && c == '\\')) {
            match part {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                other => segments.push(other.to_string()),
            }
        }
        (root, segments)
    }

    /// `path.relative(from, to)` for absolute paths.
    fn relative(self, from: &str, to: &str) -> String {
        if self == Self::Posix && !(self.is_absolute(from) && self.is_absolute(to)) {
            return crate::core::tools::path_utils::node_path_relative(from, to);
        }
        let (from_root, from_parts) = self.split_absolute(from);
        let (to_root, to_parts) = self.split_absolute(to);
        let sep = self.sep().to_string();
        let eq = |a: &str, b: &str| match self {
            Self::Posix => a == b,
            Self::Win32 => a.eq_ignore_ascii_case(b),
        };
        if !eq(&from_root, &to_root) {
            return format!("{to_root}{sep}{}", to_parts.join(&sep));
        }
        let mut common = 0;
        while common < from_parts.len() && common < to_parts.len() && eq(&from_parts[common], &to_parts[common]) {
            common += 1;
        }
        let mut parts: Vec<String> = vec!["..".to_string(); from_parts.len() - common];
        parts.extend(to_parts[common..].iter().cloned());
        parts.join(&sep)
    }
}

/// Relativize a find result against the search root and normalize it to posix separators.
pub fn relativize_find_result_path(result_path: &str, search_path: &str, path_module: Option<PathFlavor>) -> String {
    let flavor = path_module.unwrap_or_else(PathFlavor::native);
    let sep = flavor.sep();
    let had_trailing_separator = result_path.ends_with(sep) || (sep == '\\' && result_path.ends_with('/'));
    let relative_path = if flavor.is_absolute(result_path) {
        flavor.relative(search_path, result_path)
    } else {
        result_path.to_string()
    };
    let posix_path = relative_path.split(sep).collect::<Vec<_>>().join("/");
    if had_trailing_separator && !posix_path.ends_with('/') {
        format!("{posix_path}/")
    } else {
        posix_path
    }
}

const FIND_SNIPPET: &str = "Find files by glob pattern (respects .gitignore)";

pub static FIND_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": FIND_SNIPPET, "guidelines": [] }));

const DEFAULT_LIMIT: f64 = 1000.0;

fn find_schema() -> Value {
    json!({
        "type": "object",
        "required": ["pattern"],
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
            },
            "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
            "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindToolInput {
    /// Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'
    pub pattern: String,
    /// Directory to search in (default: current directory)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum number of results (default: 1000)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub result_limit_reached: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindOperationsOptions {
    pub ignore: Vec<String>,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub limit: f64,
}

/// Pluggable operations for the find tool.
/// Override these to delegate file search to remote systems (for example SSH).
#[derive(Clone)]
pub struct FindOperations {
    /// Check if path exists
    pub exists: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>,
    /// Find files matching glob pattern. Returns relative or absolute paths.
    pub glob: Arc<
        dyn Fn(
                String,
                String,
                FindOperationsOptions,
            ) -> BoxFuture<'static, anyhow::Result<Vec<String>>>
            + Send
            + Sync,
    >,
}

#[derive(Clone, Default)]
pub struct FindToolOptions {
    /// Custom operations for find. Default: local filesystem plus fd
    pub operations: Option<FindOperations>,
}

fn build_find_result(
    relativized: Vec<String>,
    effective_limit: f64,
    limit_notice: impl Fn(f64) -> String,
) -> anyhow::Result<AgentToolResult> {
    let result_limit_reached = relativized.len() as f64 >= effective_limit;
    let raw_output = relativized.join("\n");
    let truncation = truncate_head(
        &raw_output,
        Some(&TruncationOptions {
            max_lines: Some(MAX_SAFE_INTEGER),
            max_bytes: None,
        }),
    );
    let mut result_output = truncation.content.clone();
    let mut details = FindToolDetails::default();
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        notices.push(limit_notice(effective_limit));
        details.result_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if !notices.is_empty() {
        result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let has_details = details.result_limit_reached.is_some() || details.truncation.is_some();
    Ok(AgentToolResult {
        content: vec![UserContentBlock::text(result_output)],
        details: if has_details { Some(serde_json::to_value(details)?) } else { None },
        ..Default::default()
    })
}

fn no_files_found() -> AgentToolResult {
    AgentToolResult {
        content: vec![UserContentBlock::text("No files found matching pattern")],
        details: None,
        ..Default::default()
    }
}

async fn execute_custom_glob(
    ops: &FindOperations,
    search_path: &str,
    pattern: &str,
    effective_limit: f64,
    signal: &CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    if !(ops.exists)(search_path.to_string()).await? {
        anyhow::bail!("Path not found: {search_path}");
    }
    if signal.is_cancelled() {
        anyhow::bail!("Operation aborted");
    }
    let results = (ops.glob)(
        pattern.to_string(),
        search_path.to_string(),
        FindOperationsOptions {
            ignore: vec!["**/node_modules/**".to_string(), "**/.git/**".to_string()],
            limit: effective_limit,
        },
    )
    .await?;
    if signal.is_cancelled() {
        anyhow::bail!("Operation aborted");
    }
    if results.is_empty() {
        return Ok(no_files_found());
    }
    // Relativize paths against the search root for stable output.
    let relativized: Vec<String> = results
        .iter()
        .map(|p| relativize_find_result_path(p, search_path, None))
        .collect();
    build_find_result(relativized, effective_limit, |limit| {
        format!("{} results limit reached", js_number_string(limit))
    })
}

async fn execute_fd(
    search_path: &str,
    pattern: &str,
    effective_limit: f64,
    signal: &CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    let fd_path = ensure_tool(GetToolPathTool::Fd, None).await?;
    if signal.is_cancelled() {
        anyhow::bail!("Operation aborted");
    }
    let Some(fd_path) = fd_path else {
        anyhow::bail!("fd is not available and could not be downloaded");
    };

    let mut args: Vec<String> = vec!["--glob".into(), "--color=never".into(), "--hidden".into()];

    // fd normally ignores .gitignore outside git repos, so keep --no-require-git
    // there. Inside repos, use fd's default git-aware behavior so parent
    // .gitignore rules stop at nested repo boundaries:
    // https://github.com/earendil-works/pi/issues/5960
    let mut inside_git_repo = false;
    let mut current = std::path::PathBuf::from(search_path);
    loop {
        if path_exists(&current.join(".git").to_string_lossy()).await? {
            inside_git_repo = true;
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    if !inside_git_repo {
        args.push("--no-require-git".into());
    }
    args.push("--max-results".into());
    args.push(js_number_string(effective_limit));

    // fd --glob matches against the basename unless --full-path is set; in --full-path
    // mode it matches against the absolute candidate path, so a path-containing
    // pattern like 'src/**/*.spec.ts' needs a leading '**/' to match anything.
    let mut effective_pattern = pattern.to_string();
    if pattern.contains('/') {
        args.push("--full-path".into());
        if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
            effective_pattern = format!("**/{pattern}");
        }
        // fd matches full paths using native separators on Windows.
        if cfg!(windows) {
            effective_pattern = effective_pattern.replace('/', "[/\\\\]");
        }
    }
    args.push("--".into());
    args.push(effective_pattern);
    args.push(search_path.to_string());

    let mut child = match tokio::process::Command::new(&fd_path)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let (code, _) = crate::core::tools::node_fs::io_error_code(&err);
            anyhow::bail!("Failed to run fd: spawn {fd_path} {code}");
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut reader = tokio::io::BufReader::new(stdout).lines();
    let mut lines: Vec<String> = Vec::new();
    loop {
        tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(line)) => lines.push(line),
                _ => break,
            },
            _ = signal.cancelled() => {
                terminate_child(&child);
                anyhow::bail!("Operation aborted");
            }
        }
    }
    let status = child.wait().await;
    let stderr = stderr_task.await.unwrap_or_default();
    if signal.is_cancelled() {
        anyhow::bail!("Operation aborted");
    }
    let code = match status {
        Ok(status) => status.code(),
        Err(err) => anyhow::bail!("Failed to run fd: {err}"),
    };
    let output = lines.join("\n");
    if code != Some(0) {
        let trimmed = stderr.trim();
        let error_msg = if trimmed.is_empty() {
            format!(
                "fd exited with code {}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "null".to_string())
            )
        } else {
            trimmed.to_string()
        };
        if output.is_empty() {
            anyhow::bail!(error_msg);
        }
    }
    if output.is_empty() {
        return Ok(no_files_found());
    }

    let mut relativized: Vec<String> = Vec::new();
    for raw_line in &lines {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        relativized.push(relativize_find_result_path(line, search_path, None));
    }
    build_find_result(relativized, effective_limit, |limit| {
        format!(
            "{} results limit reached. Use limit={} for more, or refine pattern",
            js_number_string(limit),
            js_number_string(limit * 2.0)
        )
    })
}

pub fn create_find_tool_definition(cwd: &str, options: Option<&FindToolOptions>) -> ToolDefinition {
    let custom_ops = options.and_then(|o| o.operations.clone());
    let cwd = cwd.to_string();
    let mut definition = ToolDefinition::new(
        "find",
        "find",
        format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {} results or {}KB (whichever is hit first).",
            js_number_string(DEFAULT_LIMIT),
            DEFAULT_MAX_BYTES / 1024
        ),
        find_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let custom_ops = custom_ops.clone();
            let cwd = cwd.clone();
            async move {
                let input: FindToolInput = parse_tool_input("find", params)?;
                let signal = signal.unwrap_or_default();
                if signal.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                let search_path = resolve_to_cwd(
                    input.path.as_deref().filter(|p| !p.is_empty()).unwrap_or("."),
                    &effective_cwd(ctx.as_ref(), &cwd),
                );
                let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT);
                let work = async {
                    match &custom_ops {
                        // If custom operations provide glob(), use that instead of fd.
                        Some(ops) => execute_custom_glob(ops, &search_path, &input.pattern, effective_limit, &signal).await,
                        None => execute_fd(&search_path, &input.pattern, effective_limit, &signal).await,
                    }
                };
                let result = tokio::select! {
                    result = work => result,
                    _ = signal.cancelled() => anyhow::bail!("Operation aborted"),
                };
                if result.is_err() && signal.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                result
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(FIND_SNIPPET.to_string());
    let renderers = crate::core::tools::renderers::find::find_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_find_tool(cwd: &str, options: Option<&FindToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_find_tool_definition(cwd, options), None)
}
