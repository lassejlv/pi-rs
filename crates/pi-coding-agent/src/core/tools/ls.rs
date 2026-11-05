//! Port of `packages/coding-agent/src/core/tools/ls.ts`.

use crate::core::extensions::types::ToolDefinition;
use crate::core::tools::bash::{effective_cwd, parse_tool_input};
use crate::core::tools::js_util::{js_number_string, locale_compare};
use crate::core::tools::path_utils::{node_path_join, resolve_to_cwd};
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::core::tools::truncate::{
    DEFAULT_MAX_BYTES, MAX_SAFE_INTEGER, TruncationOptions, TruncationResult, format_size, truncate_head,
};
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::types::UserContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::LazyLock;

const LS_SNIPPET: &str = "List directory contents";

pub static LS_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": LS_SNIPPET, "guidelines": [] }));

const DEFAULT_LIMIT: f64 = 500.0;

fn ls_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: current directory)" },
            "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsToolInput {
    /// Directory to list (default: current directory)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Maximum number of entries to return (default: 500)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub entry_limit_reached: Option<f64>,
}

#[derive(Clone)]
pub struct LsOperationsResult {
    pub is_directory: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// Pluggable operations for the ls tool.
/// Override these to delegate directory listing to remote systems (for example SSH).
#[derive(Clone)]
pub struct LsOperations {
    /// Check if path exists
    pub exists: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>,
    /// Get file or directory stats. Throws if not found.
    pub stat:
        Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<LsOperationsResult>> + Send + Sync>,
    /// Read directory entries
    pub readdir:
        Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Vec<String>>> + Send + Sync>,
}

#[derive(Clone, Default)]
pub struct LsToolOptions {
    /// Custom operations for directory listing. Default: local filesystem
    pub operations: Option<LsOperations>,
}

fn default_ls_operations() -> LsOperations {
    use crate::core::tools::node_fs;
    LsOperations {
        exists: Arc::new(|path| Box::pin(async move { crate::core::tools::path_utils::path_exists(&path).await })),
        stat: Arc::new(|path| {
            Box::pin(async move {
                let meta = node_fs::stat(&path).await?;
                let is_dir = meta.is_dir();
                Ok(LsOperationsResult {
                    is_directory: Arc::new(move || is_dir),
                })
            })
        }),
        readdir: Arc::new(|path| Box::pin(async move { Ok(node_fs::readdir(&path).await?) })),
    }
}

async fn execute_ls(ops: &LsOperations, cwd: &str, input: LsToolInput) -> anyhow::Result<AgentToolResult> {
    let dir_path = resolve_to_cwd(input.path.as_deref().filter(|p| !p.is_empty()).unwrap_or("."), cwd);
    let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT);

    // Check if path exists.
    if !(ops.exists)(dir_path.clone()).await? {
        anyhow::bail!("Path not found: {dir_path}");
    }

    // Check if path is a directory.
    let stat = (ops.stat)(dir_path.clone()).await?;
    if !(stat.is_directory)() {
        anyhow::bail!("Not a directory: {dir_path}");
    }

    // Read directory entries.
    let mut entries = match (ops.readdir)(dir_path.clone()).await {
        Ok(entries) => entries,
        Err(err) => anyhow::bail!("Cannot read directory: {err}"),
    };

    // Sort alphabetically, case-insensitive.
    entries.sort_by(|a, b| locale_compare(&a.to_lowercase(), &b.to_lowercase()));

    // Format entries with directory indicators.
    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in entries {
        if results.len() as f64 >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = node_path_join(&[&dir_path, &entry]);
        let suffix = match (ops.stat)(full_path).await {
            Ok(entry_stat) => {
                if (entry_stat.is_directory)() {
                    "/"
                } else {
                    ""
                }
            }
            // Skip entries we cannot stat.
            Err(_) => continue,
        };
        results.push(format!("{entry}{suffix}"));
    }

    if results.is_empty() {
        return Ok(AgentToolResult {
            content: vec![UserContentBlock::text("(empty directory)")],
            details: None,
            ..Default::default()
        });
    }

    let raw_output = results.join("\n");
    // Apply byte truncation. There is no separate line limit because entry count is already capped.
    let truncation = truncate_head(
        &raw_output,
        Some(&TruncationOptions {
            max_lines: Some(MAX_SAFE_INTEGER),
            max_bytes: None,
        }),
    );
    let mut output = truncation.content.clone();
    let mut details = LsToolDetails::default();
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{} entries limit reached. Use limit={} for more",
            js_number_string(effective_limit),
            js_number_string(effective_limit * 2.0)
        ));
        details.entry_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    let has_details = details.entry_limit_reached.is_some() || details.truncation.is_some();
    Ok(AgentToolResult {
        content: vec![UserContentBlock::text(output)],
        details: if has_details { Some(serde_json::to_value(details)?) } else { None },
        ..Default::default()
    })
}

pub fn create_ls_tool_definition(cwd: &str, options: Option<&LsToolOptions>) -> ToolDefinition {
    let ops = options
        .and_then(|o| o.operations.clone())
        .unwrap_or_else(default_ls_operations);
    let cwd = cwd.to_string();
    let mut definition = ToolDefinition::new(
        "ls",
        "ls",
        format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {} entries or {}KB (whichever is hit first).",
            js_number_string(DEFAULT_LIMIT),
            DEFAULT_MAX_BYTES / 1024
        ),
        ls_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let ops = ops.clone();
            let cwd = cwd.clone();
            async move {
                let input: LsToolInput = parse_tool_input("ls", params)?;
                let signal = signal.unwrap_or_default();
                if signal.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                let cwd = effective_cwd(ctx.as_ref(), &cwd);
                tokio::select! {
                    result = execute_ls(&ops, &cwd, input) => result,
                    _ = signal.cancelled() => anyhow::bail!("Operation aborted"),
                }
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(LS_SNIPPET.to_string());
    let renderers = crate::core::tools::renderers::ls::ls_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_ls_tool(cwd: &str, options: Option<&LsToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_ls_tool_definition(cwd, options), None)
}
