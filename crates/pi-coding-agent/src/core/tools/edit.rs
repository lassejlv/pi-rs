//! Port of `packages/coding-agent/src/core/tools/edit.ts`.

use crate::core::extensions::types::{RenderShell, ToolDefinition};
use crate::core::tools::bash::effective_cwd;
use crate::core::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string, generate_unified_patch,
    normalize_to_lf, restore_line_endings,
};
use crate::core::tools::file_mutation_queue::with_file_mutation_queue;
use crate::core::tools::node_fs::describe_error_with_code;
use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::utils::text::{SplitBomResult, split_bom};
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::types::{ConstrainedSamplingConfig, JsonSchemaStrictness, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use std::sync::Arc;
use std::sync::LazyLock;

const EDIT_SNIPPET: &str =
    "Make precise file edits with exact text replacement, including multiple disjoint edits in one call";
const EDIT_GUIDELINES: &[&str] = &[
    "Use edit for precise changes (edits[].oldText must match exactly)",
    "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
    "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
    "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
];

pub static EDIT_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": EDIT_SNIPPET, "guidelines": EDIT_GUIDELINES }));

fn edit_schema() -> Value {
    json!({
        "type": "object",
        "required": ["path", "edits"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["oldText", "newText"],
                    "properties": {
                        "oldText": {
                            "type": "string",
                            "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                        },
                        "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                    }
                },
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
            }
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolInputEdits {
    /// Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.
    pub old_text: String,
    /// Replacement text for this targeted edit.
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolInput {
    /// Path to the file to edit (relative or absolute)
    pub path: String,
    /// One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.
    pub edits: Vec<EditToolInputEdits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyEditToolInputValue {
    #[serde(flatten)]
    pub base: EditToolInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<Value>,
}

pub type LegacyEditToolInput = LegacyEditToolInputValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleEditInput {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    /// Display-oriented diff of the changes made
    pub diff: String,
    /// Standard unified patch of the changes made
    pub patch: String,
    /// Line number of the first change in the new file (for editor navigation)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<i64>,
}

/// Pluggable operations for the edit tool.
/// Override these to delegate file editing to remote systems (for example SSH).
#[derive(Clone)]
pub struct EditOperations {
    /// Read file contents as a Buffer
    pub read_file: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> + Send + Sync>,
    /// Write content to a file
    pub write_file:
        Arc<dyn Fn(String, String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
    /// Check if file is readable and writable (throw if not)
    pub access: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
}

#[derive(Clone, Default)]
pub struct EditToolOptions {
    /// Custom operations for file editing. Default: local filesystem
    pub operations: Option<EditOperations>,
}

fn default_edit_operations() -> EditOperations {
    use crate::core::tools::node_fs;
    EditOperations {
        read_file: Arc::new(|path| Box::pin(async move { Ok(node_fs::read_file(&path).await?) })),
        write_file: Arc::new(|path, content| {
            Box::pin(async move { Ok(node_fs::write_file(&path, content.as_bytes()).await?) })
        }),
        access: Arc::new(|path| {
            Box::pin(async move { Ok(node_fs::access(&path, node_fs::R_OK | node_fs::W_OK).await?) })
        }),
    }
}

fn is_single_edit_input(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("oldText").is_some_and(Value::is_string) && map.get("newText").is_some_and(Value::is_string)
        }
        _ => false,
    }
}

/// Compatibility shim run before schema validation (TS `prepareEditArguments`).
pub fn prepare_edit_arguments(input: Value) -> Value {
    let Value::Object(mut args) = input else {
        return input;
    };

    // Some models (Opus 4.6, GLM-5.1) send edits as a JSON string instead of an array.
    // Others send a single edit object instead of a one-element edits array.
    match args.get("edits") {
        Some(Value::String(text)) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                if parsed.is_array() {
                    args.insert("edits".to_string(), parsed);
                } else if is_single_edit_input(&parsed) {
                    args.insert("edits".to_string(), Value::Array(vec![parsed]));
                }
            }
        }
        Some(edits) if is_single_edit_input(edits) => {
            let edit = edits.clone();
            args.insert("edits".to_string(), Value::Array(vec![edit]));
        }
        _ => {}
    }

    let (Some(Value::String(old_text)), Some(Value::String(new_text))) =
        (args.get("oldText").cloned(), args.get("newText").cloned())
    else {
        return Value::Object(args);
    };

    let mut edits = match args.get("edits") {
        Some(Value::Array(edits)) => edits.clone(),
        _ => Vec::new(),
    };
    edits.push(json!({ "oldText": old_text, "newText": new_text }));
    args.shift_remove("oldText");
    args.shift_remove("newText");
    args.insert("edits".to_string(), Value::Array(edits));
    Value::Object(args)
}

fn validate_edit_input(input: &Value) -> anyhow::Result<(String, Vec<Edit>)> {
    let edits = match input.get("edits") {
        Some(Value::Array(edits)) if !edits.is_empty() => edits,
        _ => anyhow::bail!("Edit tool input is invalid. edits must contain at least one replacement."),
    };
    let input: EditToolInput = serde_json::from_value(json!({ "path": input.get("path").cloned().unwrap_or(Value::Null), "edits": edits }))
        .map_err(|err| anyhow::anyhow!("Invalid arguments for edit: {err}"))?;
    Ok((
        input.path,
        input
            .edits
            .into_iter()
            .map(|e| Edit {
                old_text: e.old_text,
                new_text: e.new_text,
            })
            .collect(),
    ))
}

async fn execute_edit(
    ops: EditOperations,
    absolute_path: String,
    path: String,
    edits: Vec<Edit>,
    signal: CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    with_file_mutation_queue(&absolute_path, || async {
        // Do not abort from a listener here: that would release the mutation queue while an
        // in-flight filesystem operation may still finish. Checking the signal after each await
        // observes the same aborts while keeping the queue locked until the operation has settled.
        let throw_if_aborted = || -> anyhow::Result<()> {
            if signal.is_cancelled() {
                anyhow::bail!("Operation aborted");
            }
            Ok(())
        };

        throw_if_aborted()?;

        // Check if file exists.
        if let Err(error) = (ops.access)(absolute_path.clone()).await {
            throw_if_aborted()?;
            anyhow::bail!("Could not edit file: {path}. {}.", describe_error_with_code(&error));
        }
        throw_if_aborted()?;

        // Read the file.
        let buffer = (ops.read_file)(absolute_path.clone()).await?;
        let raw_content = crate::core::tools::output_accumulator::decode_utf8_lossy(&buffer);
        throw_if_aborted()?;

        // Strip BOM before matching. The model will not include an invisible BOM in oldText.
        let SplitBomResult { bom, text: content } = split_bom(&raw_content);
        let original_ending = detect_line_ending(&content);
        let normalized_content = normalize_to_lf(&content);
        let applied = apply_edits_to_normalized_content(&normalized_content, &edits, &path)?;
        throw_if_aborted()?;

        let final_content = format!("{bom}{}", restore_line_endings(&applied.new_content, original_ending));
        (ops.write_file)(absolute_path.clone(), final_content).await?;
        throw_if_aborted()?;

        let diff_result = generate_diff_string(&applied.base_content, &applied.new_content, None);
        let patch = generate_unified_patch(&path, &applied.base_content, &applied.new_content, None);
        let details = EditToolDetails {
            diff: diff_result.diff,
            patch,
            first_changed_line: diff_result.first_changed_line,
        };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Successfully replaced {} block(s) in {path}.",
                edits.len()
            ))],
            details: Some(serde_json::to_value(details)?),
            ..Default::default()
        })
    })
    .await
}

pub fn create_edit_tool_definition(cwd: &str, options: Option<&EditToolOptions>) -> ToolDefinition {
    let ops = options
        .and_then(|o| o.operations.clone())
        .unwrap_or_else(default_edit_operations);
    let cwd = cwd.to_string();
    let mut definition = ToolDefinition::new(
        "edit",
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        edit_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let ops = ops.clone();
            let cwd = cwd.clone();
            async move {
                let (path, edits) = validate_edit_input(&params)?;
                let absolute_path = resolve_to_cwd(&path, &effective_cwd(ctx.as_ref(), &cwd));
                execute_edit(ops, absolute_path, path, edits, signal.unwrap_or_default()).await
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(EDIT_SNIPPET.to_string());
    definition.prompt_guidelines = Some(EDIT_GUIDELINES.iter().map(|s| s.to_string()).collect());
    definition.constrained_sampling = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: JsonSchemaStrictness::Prefer,
    });
    definition.render_shell = Some(RenderShell::SelfRendered);
    definition.prepare_arguments = Some(Arc::new(prepare_edit_arguments));
    let renderers = crate::core::tools::renderers::edit::edit_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_edit_tool(cwd: &str, options: Option<&EditToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_edit_tool_definition(cwd, options), None)
}
