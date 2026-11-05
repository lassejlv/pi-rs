//! Port of `packages/coding-agent/src/core/tools/read.ts`.

use crate::core::extensions::types::{ExtensionToolContext, ToolDefinition};
use crate::core::tools::bash::{effective_cwd, parse_tool_input};
use crate::core::tools::js_util::js_number_string;
use crate::core::tools::path_utils::resolve_read_path_async;
use crate::core::tools::tool_definition_wrapper::wrap_tool_definition;
use crate::core::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationResult, TruncationResultTruncatedBy, format_size, truncate_head,
};
use crate::utils::image_process::{ProcessImageOptions, ProcessImageResult, process_image};
use crate::utils::image_resize_core::ImageResizeOptions;
use futures::future::BoxFuture;
use pi_agent::types::{AgentToolRef, AgentToolResult};
use pi_ai::providers::model_schema::ModelImageResizeOptions;
use pi_ai::types::{ConstrainedSamplingConfig, JsonSchemaStrictness, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::LazyLock;

const READ_SNIPPET: &str = "Read file contents";
const READ_GUIDELINES: &[&str] = &["Use read to examine files instead of cat or sed."];

pub static READ_TOOL_SYSTEM_PROMPT_CONTRIBUTION: LazyLock<Value> =
    LazyLock::new(|| json!({ "snippet": READ_SNIPPET, "guidelines": READ_GUIDELINES }));

fn read_schema() -> Value {
    json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "number", "description": "Maximum number of lines to read" }
        }
    })
}

/// Result for programmatic callers such as codemode scripts (TypeBox `readOutputSchema`).
fn read_output_schema() -> Value {
    json!({
        "anyOf": [
            { "type": "string" },
            {
                "type": "object",
                "required": ["type", "data", "mimeType", "note"],
                "properties": {
                    "type": { "type": "string", "const": "image" },
                    "data": { "type": "string" },
                    "mimeType": { "type": "string" },
                    "note": { "type": "string" }
                }
            }
        ]
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolInput {
    /// Path to the file to read (relative or absolute)
    pub path: String,
    /// Line number to start reading from (1-indexed)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub offset: Option<f64>,
    /// Maximum number of lines to read
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolOutputVariant2 {
    /// Always "image".
    pub r#type: String,
    pub data: String,
    pub mime_type: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadToolOutputValue {
    String(String),
    ReadToolOutputVariant2(ReadToolOutputVariant2),
}

pub type ReadToolOutput = ReadToolOutputValue;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
}

/// Pluggable operations for the read tool.
/// Override these to delegate file reading to remote systems (for example SSH).
#[derive(Clone)]
pub struct ReadOperations {
    /// Read file contents as a Buffer
    pub read_file: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> + Send + Sync>,
    /// Check if file is readable (throw if not)
    pub access: Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
    /// Detect image MIME type, return null or undefined for non-images
    pub detect_image_mime_type: Option<
        Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>,
    >,
}

#[derive(Clone, Default)]
pub struct ReadToolOptions {
    /// Whether to auto-resize images. Default: true
    pub auto_resize_images: Option<bool>,
    /// Fallback resize profile when the execution context has no model metadata.
    pub resize_options: Option<ModelImageResizeOptions>,
    /// Custom operations for file reading. Default: local filesystem
    pub operations: Option<ReadOperations>,
}

fn default_read_operations() -> ReadOperations {
    use crate::core::tools::node_fs;
    ReadOperations {
        read_file: Arc::new(|path| Box::pin(async move { Ok(node_fs::read_file(&path).await?) })),
        access: Arc::new(|path| Box::pin(async move { Ok(node_fs::access(&path, node_fs::R_OK).await?) })),
        detect_image_mime_type: Some(Arc::new(|path| {
            Box::pin(async move { crate::utils::mime::detect_supported_image_mime_type_from_file(&path).await })
        })),
    }
}

/// The image block and its note, or the text for text files and images that could not be processed.
fn to_read_output(content: &[UserContentBlock]) -> ReadToolOutput {
    let text = content
        .iter()
        .find_map(UserContentBlock::as_text)
        .map(|t| t.text.clone())
        .unwrap_or_default();
    match content.iter().find_map(UserContentBlock::as_image) {
        Some(image) => ReadToolOutputValue::ReadToolOutputVariant2(ReadToolOutputVariant2 {
            r#type: "image".to_string(),
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
            note: text,
        }),
        None => ReadToolOutputValue::String(text),
    }
}

fn get_non_vision_image_note(model: Option<&pi_ai::types::Model>) -> Option<String> {
    let model = model?;
    if model.input.contains(&pi_ai::providers::model_schema::ModelInputModality::Image) {
        return None;
    }
    Some("[Current model does not support images. The image will be omitted from this request.]".to_string())
}

/// `ModelImageResizeOptions` as resize options for `processImage`.
pub(crate) fn to_image_resize_options(options: &ModelImageResizeOptions) -> ImageResizeOptions {
    ImageResizeOptions {
        max_width: options.max_width.map(|v| v as i64),
        max_height: options.max_height.map(|v| v as i64),
        max_bytes: options.max_bytes.map(|v| v as i64),
        jpeg_quality: options.jpeg_quality.map(|v| v as f64),
    }
}

/// `array.slice(start, end)` index bounds for an array of `len` items (ToIntegerOrInfinity
/// truncation, negative offsets from the end).
fn js_slice_bounds(len: usize, start: f64, end: Option<f64>) -> (usize, usize) {
    let clamp = |value: f64| -> usize {
        let value = if value.is_nan() { 0.0 } else { value.trunc() };
        if value < 0.0 {
            (len as f64 + value).max(0.0) as usize
        } else {
            value.min(len as f64) as usize
        }
    };
    let from = clamp(start);
    let to = end.map(clamp).unwrap_or(len);
    (from, to.max(from))
}

struct ReadExecution {
    cwd: String,
    auto_resize_images: bool,
    fallback_resize_options: Option<ModelImageResizeOptions>,
    ops: ReadOperations,
}

impl ReadExecution {
    async fn run(
        &self,
        input: ReadToolInput,
        ctx: Option<&ExtensionToolContext>,
    ) -> anyhow::Result<(Vec<UserContentBlock>, Option<ReadToolDetails>)> {
        let path = input.path.clone();
        let absolute_path = resolve_read_path_async(&path, &effective_cwd(ctx, &self.cwd)).await?;
        // Check if file exists and is readable.
        (self.ops.access)(absolute_path.clone()).await?;
        let mime_type = match &self.ops.detect_image_mime_type {
            Some(detect) => detect(absolute_path.clone()).await?,
            None => None,
        };
        let model = ctx.and_then(|ctx| ctx.model().ok().flatten());
        let non_vision_image_note = get_non_vision_image_note(model.as_ref());

        if let Some(mime_type) = mime_type.filter(|m| !m.is_empty()) {
            // Read image as binary.
            let buffer = (self.ops.read_file)(absolute_path.clone()).await?;
            let resize_options = model
                .as_ref()
                .and_then(|m| m.input_limits.as_ref())
                .and_then(|l| l.images.as_ref())
                .and_then(|i| i.resize.as_ref())
                .or(self.fallback_resize_options.as_ref())
                .map(to_image_resize_options);
            let processed = process_image(
                &buffer,
                &mime_type,
                Some(&ProcessImageOptions {
                    auto_resize_images: Some(self.auto_resize_images),
                    resize_options,
                }),
            )
            .await?;
            let content = match processed {
                ProcessImageResult::ProcessImageResultVariant2(failure) => {
                    let mut text_note = format!("Read image file [{mime_type}]\n{}", failure.message);
                    if let Some(note) = &non_vision_image_note {
                        text_note.push_str(&format!("\n{note}"));
                    }
                    vec![UserContentBlock::text(text_note)]
                }
                ProcessImageResult::ProcessImageResultVariant1(processed) => {
                    let mut text_note = format!("Read image file [{}]", processed.mime_type);
                    if !processed.hints.is_empty() {
                        text_note.push_str(&format!("\n{}", processed.hints.join("\n")));
                    }
                    if let Some(note) = &non_vision_image_note {
                        text_note.push_str(&format!("\n{note}"));
                    }
                    vec![
                        UserContentBlock::text(text_note),
                        UserContentBlock::Image(pi_ai::types::ImageContent {
                            data: processed.data,
                            mime_type: processed.mime_type,
                        }),
                    ]
                }
            };
            return Ok((content, None));
        }

        // Read text content.
        let buffer = (self.ops.read_file)(absolute_path).await?;
        let text_content = crate::core::tools::output_accumulator::decode_utf8_lossy(&buffer);
        let all_lines: Vec<&str> = text_content.split('\n').collect();
        let total_file_lines = all_lines.len();
        // Apply offset if specified. Convert from 1-indexed input to 0-indexed array access.
        let start_line: f64 = match input.offset {
            Some(offset) if offset != 0.0 && !offset.is_nan() => (offset - 1.0).max(0.0),
            _ => 0.0,
        };
        let start_line_display = start_line + 1.0;
        if start_line >= all_lines.len() as f64 {
            anyhow::bail!(
                "Offset {} is beyond end of file ({} lines total)",
                input.offset.map(js_number_string).unwrap_or_else(|| "undefined".to_string()),
                all_lines.len()
            );
        }

        let (selected_content, user_limited_lines) = match input.limit {
            Some(limit) => {
                let end_line = (start_line + limit).min(all_lines.len() as f64);
                let (from, to) = js_slice_bounds(all_lines.len(), start_line, Some(end_line));
                (all_lines[from..to].join("\n"), Some(end_line - start_line))
            }
            None => {
                let (from, to) = js_slice_bounds(all_lines.len(), start_line, None);
                (all_lines[from..to].join("\n"), None)
            }
        };

        // Apply truncation, respecting both line and byte limits.
        let truncation = truncate_head(&selected_content, None);
        let mut details = None;
        let output_text = if truncation.first_line_exceeds_limit {
            // First line alone exceeds the byte limit. Point the model at a bash fallback.
            let first_line = all_lines[start_line.trunc() as usize];
            let first_line_size = format_size(first_line.len() as i64);
            let display = js_number_string(start_line_display);
            let text = format!(
                "[Line {display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES)
            );
            details = Some(ReadToolDetails {
                truncation: Some(truncation),
            });
            text
        } else if truncation.truncated {
            // Truncation occurred. Build an actionable continuation notice.
            let end_line_display = start_line_display + truncation.output_lines as f64 - 1.0;
            let next_offset = end_line_display + 1.0;
            let mut text = truncation.content.clone();
            if truncation.truncated_by == Some(TruncationResultTruncatedBy::Lines) {
                text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {total_file_lines}. Use offset={} to continue.]",
                    js_number_string(start_line_display),
                    js_number_string(end_line_display),
                    js_number_string(next_offset)
                ));
            } else {
                text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {total_file_lines} ({} limit). Use offset={} to continue.]",
                    js_number_string(start_line_display),
                    js_number_string(end_line_display),
                    format_size(DEFAULT_MAX_BYTES),
                    js_number_string(next_offset)
                ));
            }
            details = Some(ReadToolDetails {
                truncation: Some(truncation),
            });
            text
        } else if let Some(user_limited) = user_limited_lines
            && start_line + user_limited < all_lines.len() as f64
        {
            // User-specified limit stopped early, but the file still has more content.
            let remaining = all_lines.len() as f64 - (start_line + user_limited);
            let next_offset = start_line + user_limited + 1.0;
            format!(
                "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                truncation.content,
                js_number_string(remaining),
                js_number_string(next_offset)
            )
        } else {
            truncation.content
        };
        Ok((vec![UserContentBlock::text(output_text)], details))
    }
}

pub fn create_read_tool_definition(cwd: &str, options: Option<&ReadToolOptions>) -> ToolDefinition {
    let execution = Arc::new(ReadExecution {
        cwd: cwd.to_string(),
        auto_resize_images: options.and_then(|o| o.auto_resize_images).unwrap_or(true),
        fallback_resize_options: options.and_then(|o| o.resize_options.clone()),
        ops: options
            .and_then(|o| o.operations.clone())
            .unwrap_or_else(default_read_operations),
    });
    let mut definition = ToolDefinition::new(
        "read",
        "read",
        format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        ),
        read_schema(),
        move |_tool_call_id, params, signal, _on_update, ctx| {
            let execution = execution.clone();
            async move {
                let input: ReadToolInput = parse_tool_input("read", params)?;
                let signal = signal.unwrap_or_default();
                if signal.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                let (content, details) = tokio::select! {
                    result = execution.run(input, ctx.as_ref()) => result?,
                    _ = signal.cancelled() => anyhow::bail!("Operation aborted"),
                };
                let structured = to_read_output(&content);
                Ok(AgentToolResult {
                    content,
                    details: details.map(|d| serde_json::to_value(d).unwrap_or(Value::Null)),
                    structured_content: Some(serde_json::to_value(structured)?),
                    ..Default::default()
                })
            }
        },
    );
    // TS builds the schema with TypeBox.
    definition.parameters_typebox = true;
    definition.prompt_snippet = Some(READ_SNIPPET.to_string());
    definition.prompt_guidelines = Some(READ_GUIDELINES.iter().map(|s| s.to_string()).collect());
    definition.output_schema = Some(read_output_schema());
    definition.constrained_sampling = Some(ConstrainedSamplingConfig::JsonSchema {
        strict: JsonSchemaStrictness::Prefer,
    });
    let renderers = crate::core::tools::renderers::read::read_renderers();
    definition.render_call = renderers.render_call;
    definition.render_result = renderers.render_result;
    definition
}

pub fn create_read_tool(cwd: &str, options: Option<&ReadToolOptions>) -> AgentToolRef {
    wrap_tool_definition(&create_read_tool_definition(cwd, options), None)
}
