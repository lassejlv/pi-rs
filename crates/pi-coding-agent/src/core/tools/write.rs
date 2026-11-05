//! Port of `packages/coding-agent/src/core/tools/write.ts`.

use crate::core::extensions::types::ToolDefinition;
use crate::core::tools::bash::{effective_cwd, parse_tool_input};
use crate::core::tools::file_mutation_queue::with_file_mutation_queue;
use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::types::{ConstrainedSamplingConfig, JsonSchemaStrictness, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::LazyLock;

const WRITE_SNIPPET: &str = "Create or overwrite files";
const WRITE_GUIDELINES: &[&str] = &["Use write only for new files or complete rewrites."];

pub static WRITE_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": WRITE_SNIPPET, "guidelines": WRITE_GUIDELINES }));

fn write_schema() -> Value {
    json!({
        "type": "object",
        "required": ["path", "content"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteToolInput {
    /// Path to the file to write (relative or absolute)
    pub path: String,
    /// Content to write to the file
    pub content: String,
}

/// Pluggable operations for the write tool.
/// Override these to delegate file writing to remote systems (for example SSH).
#[derive(Clone)]
pub struct WriteOperations {
    /// Write content to a file
    pub write_file:
        Arc<dyn Fn(String, String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
    /// Create directory recursively
    pub mkdir: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
}

#[derive(Clone, Default)]
pub struct WriteToolOptions {
    /// Custom operations for file writing. Default: local filesystem
    pub operations: Option<WriteOperations>,
}

fn default_write_operations() -> WriteOperations {
    use crate::core::tools::node_fs;
    WriteOperations {
        write_file: Arc::new(|path, content| {
            Box::pin(async move { Ok(node_fs::write_file(&path, content.as_bytes()).await?) })
        }),
        mkdir: Arc::new(|dir| Box::pin(async move { Ok(node_fs::mkdir_recursive(&dir).await?) })),
    }
}

pub fn create_write_tool_definition(cwd: &str, options: Option<&WriteToolOptions>) -> ToolDefinition {
    let ops = options
        .and_then(|o| o.operations.clone())
        .unwrap_or_else(default_write_operations);
    let cwd = cwd.to_string();
    let mut definition = ToolDefinition::new(
        "write",
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        write_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let ops = ops.clone();
            let cwd = cwd.clone();
            async move {
                let input: WriteToolInput = parse_tool_input("write", params)?;
                let absolute_path = resolve_to_cwd(&input.path, &effective_cwd(ctx.as_ref(), &cwd));
                let dir = std::path::Path::new(&absolute_path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| absolute_path.clone());
                let signal = signal.unwrap_or_default();
                with_file_mutation_queue(&absolute_path, || async {
                    // Do not abort while a filesystem operation is in flight: that would release the
                    // mutation queue while it may still finish. Checking the signal after each await
                    // observes the same aborts while keeping the queue locked.
                    let throw_if_aborted = || -> anyhow::Result<()> {
                        if signal.is_cancelled() {
                            anyhow::bail!("Operation aborted");
                        }
                        Ok(())
                    };
                    throw_if_aborted()?;
                    // Create parent directories if needed.
                    (ops.mkdir)(dir.clone()).await?;
                    throw_if_aborted()?;
                    // Write the file contents.
                    (ops.write_file)(absolute_path.clone(), input.content.clone()).await?;
                    throw_if_aborted()?;
                    Ok(AgentToolResult {
                        content: vec![UserContentBlock::text(format!("Successfully wrote to {}", input.path))],
                        details: None,
                        ..Default::default()
                    })
                })
                .await
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(WRITE_SNIPPET.to_string());
    definition.prompt_guidelines = Some(WRITE_GUIDELINES.iter().map(|s| s.to_string()).collect());
    definition.constrained_sampling = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: JsonSchemaStrictness::Prefer,
    });
    let renderers = crate::core::tools::renderers::write::write_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_write_tool(cwd: &str, options: Option<&WriteToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_write_tool_definition(cwd, options), None)
}
