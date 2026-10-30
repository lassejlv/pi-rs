use super::{
    api::CodingToolApi, file_error, image::*, image_processor::*,
    path_utils::resolve_read_tool_path, require_env,
};
use crate::{
    env::{
        BinaryReader, FileInfo,
        decode::{RangeDecoder, StreamDecoder, starts_with_bom},
    },
    harness::{
        declaration::ToolDeclaration,
        live::{DiagnosticSeverity, ToolDiagnostic},
        tool::{NativeTool, ToolExecutionResult, text_content},
    },
    storage::{StorageFuture, line::eager},
    truncate::*,
    *,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadToolInput {
    pub path: JsonString,
    pub offset: Option<f64>,
    pub limit: Option<f64>,
}
#[derive(Clone, Default)]
pub struct ReadToolOptions {
    pub images: Option<Images>,
}
fn diagnostic(severity: DiagnosticSeverity, code: Option<&str>, message: String) -> ToolDiagnostic {
    ToolDiagnostic {
        severity,
        code: code.map(Into::into),
        message: message.into(),
    }
}
fn image_error(message: String) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Arc::new(vec![])),
        is_error: Some(true),
        diagnostics: Some(vec![diagnostic(
            DiagnosticSeverity::Error,
            Some("unsupported_image"),
            message,
        )]),
        ..Default::default()
    }
}
async fn read_image(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &JsonString,
    mime: &str,
    processor: Option<&Images>,
    model: ImageModel,
    context: &Context,
) -> Result<ToolExecutionResult> {
    let mut notes = Vec::new();
    let image = if let Some(processor) = processor {
        let bytes = reader
            .read(0.0, info.size as f64, context)
            .await
            .map_err(file_error)?;
        let Some(image) = processor
            .prepare(bytes, mime.to_owned(), model.limits)
            .await?
        else {
            return Ok(image_error(format!(
                "{path} is an image ({mime}) that cannot be prepared for the model"
            )));
        };
        if let Some(from) = &image.converted_from {
            notes.push(format!("Converted from {from} to {}.", image.mime_type));
        }
        if let Some(resized) = &image.resized {
            let scale = to_fixed(
                f64::from(resized.from.width) / f64::from(resized.to.width),
                2,
            );
            notes.push(format!("Resized from {}x{} to {}x{}. Multiply coordinates by {scale} to map them to the original.",resized.from.width,resized.from.height,resized.to.width,resized.to.height));
        }
        image
    } else {
        if !inline_image_type(mime) {
            return Ok(image_error(format!(
                "{path} is an image ({mime}) that needs converting, and no image processor is configured"
            )));
        }
        let unshrinkable = "and no image processor is configured to shrink it";
        if base64_length(info.size as f64) > model.limits.max_bytes {
            return Ok(image_error(format!(
                "{path} is an image ({mime}) of {}, too large to send, {unshrinkable}",
                format_size(info.size as f64)
            )));
        }
        let bytes = reader
            .read(0.0, info.size as f64, context)
            .await
            .map_err(file_error)?;
        let Some(size) = image_dimensions(&bytes, mime) else {
            return Ok(image_error(format!(
                "{path} is an image ({mime}) whose size cannot be read"
            )));
        };
        if f64::from(size.width) > model.limits.max_width
            || f64::from(size.height) > model.limits.max_height
        {
            return Ok(image_error(format!(
                "{path} is an image ({mime}) of {}x{}, larger than {}x{}, {unshrinkable}",
                size.width,
                size.height,
                pi_json::number_to_string(model.limits.max_width),
                pi_json::number_to_string(model.limits.max_height)
            )));
        }
        PreparedImage {
            data: to_base64(&bytes),
            mime_type: mime.to_owned(),
            resized: None,
            converted_from: None,
        }
    };
    if !model.vision {
        notes.push(
            "The current model does not support images; it sees a placeholder instead.".to_owned(),
        );
    }
    let mut message = format!("Read image file [{}].", image.mime_type);
    for note in notes {
        message.push(' ');
        message.push_str(&note);
    }
    Ok(ToolExecutionResult {
        output: Some(Arc::new(vec![image_block(image)?])),
        diagnostics: Some(vec![diagnostic(
            DiagnosticSeverity::Info,
            Some("image"),
            message,
        )]),
        ..Default::default()
    })
}
async fn read_head(
    reader: &dyn BinaryReader,
    start: u64,
    end: u64,
    skip_bom: bool,
    context: &Context,
) -> Result<String> {
    let mut decoder = RangeDecoder::default();
    let mut text = String::new();
    let mut newlines = 0;
    let mut position = if skip_bom && start == 0 { 3 } else { start };
    while position < end {
        let bytes = reader
            .read(
                position as f64,
                65536.0f64.min((end - position) as f64),
                context,
            )
            .await
            .map_err(file_error)?;
        if bytes.is_empty() {
            break;
        }
        position += bytes.len() as u64;
        let decoded = decoder.decode(&bytes, false);
        newlines += decoded.bytes().filter(|&b| b == 10).count();
        text.push_str(&decoded);
        if newlines >= DEFAULT_MAX_LINES || text.len() > DEFAULT_MAX_BYTES + 1 {
            return Ok(text);
        }
    }
    text.push_str(&decoder.decode(&[], true));
    Ok(text)
}
fn slice_index(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value.trunc() }
}
fn safe(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0
}
async fn read_text(
    reader: &dyn BinaryReader,
    args: &ReadToolInput,
    context: &Context,
) -> Result<ToolExecutionResult> {
    let start = args
        .offset
        .filter(|n| *n != 0.0 && !n.is_nan())
        .map_or(0.0, |n| (n - 1.0).max(0.0));
    let display = start + 1.0;
    let slice_start = slice_index(start);
    let scan_start = if safe(slice_start) { slice_start } else { 0.0 };
    let scan_end = args
        .limit
        .map(|limit| {
            let end = slice_index(start + limit);
            (scan_start + 1.0).max(end)
        })
        .filter(|end| safe(*end));
    let mut scan = reader
        .scan_lines(scan_start, scan_end, context)
        .await
        .map_err(file_error)?;
    let total = (scan.newlines + 1) as f64;
    if start >= total {
        return Err(Error::message(format!(
            "Offset {} is beyond end of file ({} lines total)",
            pi_json::number_to_string(args.offset.unwrap_or(f64::NAN)),
            pi_json::number_to_string(total)
        )));
    }
    let mut user_limited = None;
    let mut selected = total - slice_start;
    if let Some(limit) = args.limit {
        let sum = start + limit;
        let end = if sum.is_nan() {
            f64::NAN
        } else {
            sum.min(total)
        };
        user_limited = Some(end - start);
        let relative = slice_index(end);
        let slice_end = if relative < 0.0 {
            (total + relative).max(0.0)
        } else {
            relative
        };
        selected = (slice_end - slice_start).max(0.0);
        if selected > 0.0 && relative < 0.0 {
            scan = reader
                .scan_lines(scan_start, Some(slice_end), context)
                .await
                .map_err(file_error)?;
        }
    }
    let empty = selected == 0.0;
    let ends = !empty && scan.last_line_start == scan.end && scan.last_line_start > scan.start;
    let lines = if empty || scan.selected_bytes == 0 {
        0.0
    } else {
        selected - if ends { 1.0 } else { 0.0 }
    };
    let bytes = if empty {
        0.0
    } else {
        scan.selected_bytes as f64
    };
    let first = reader.read(0.0, 3.0, context).await.map_err(file_error)?;
    let head = if empty {
        String::new()
    } else {
        read_head(
            reader,
            scan.start,
            scan.end,
            starts_with_bom(&first),
            context,
        )
        .await?
    };
    let mut truncation = truncate_head_of(&head.clone().into(), lines, bytes, Default::default());
    let mut output = truncation.content.clone();
    let mut diagnostics = Vec::new();
    let mut details = None;
    if truncation.first_line_exceeds_limit {
        let line = if start.fract() == 0.0 {
            head.split('\n').next().unwrap_or_default()
        } else {
            ""
        };
        let line_size = if start.fract() == 0.0 {
            scan.first_line_bytes
        } else {
            0
        };
        let end =
            pi_chord::output::character_end(line.as_bytes(), DEFAULT_MAX_BYTES).min(line.len());
        let mut decoder = StreamDecoder::default();
        let mut text = decoder.decode(Some(&line.as_bytes()[..end]));
        text.push_str(&decoder.decode(None));
        output = text.into();
        diagnostics.push(diagnostic(DiagnosticSeverity::Warn,Some("truncated"),format!("Line {} is {}, exceeds the {} limit; showing its first {}. Use bash: sed -n '{}p' {} | tail -c +{}",pi_json::number_to_string(display),format_size(line_size as f64),format_size(DEFAULT_MAX_BYTES as f64),format_size(end as f64),pi_json::number_to_string(display),args.path,end+1)));
        truncation.output_bytes = end;
        truncation.output_lines = 1;
        details = Some(truncation);
    } else if truncation.truncated {
        let last = display + truncation.output_lines as f64 - 1.0;
        let limit = if truncation.truncated_by == Some("lines") {
            String::new()
        } else {
            format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES as f64))
        };
        diagnostics.push(diagnostic(
            DiagnosticSeverity::Info,
            Some("truncated"),
            format!(
                "Showing lines {}-{} of {}{}. Use offset={} to continue.",
                pi_json::number_to_string(display),
                pi_json::number_to_string(last),
                pi_json::number_to_string(total),
                limit,
                pi_json::number_to_string(last + 1.0)
            ),
        ));
        details = Some(truncation);
    } else if let Some(limited) = user_limited
        && start + limited < total
    {
        diagnostics.push(diagnostic(
            DiagnosticSeverity::Info,
            None,
            format!(
                "{} more lines in file. Use offset={} to continue.",
                pi_json::number_to_string(total - (start + limited)),
                pi_json::number_to_string(start + limited + 1.0)
            ),
        ));
    }
    let details = details.map(|truncation| {
        let mut value = wire::encode(&truncation);
        value
            .as_object_mut()
            .unwrap()
            .shift_remove(&JsonString::from("content"));
        let mut object = JsonObject::default();
        object.insert("truncation".into(), value);
        object.into_value().into()
    });
    Ok(ToolExecutionResult {
        output: Some(Arc::new(if output.units().is_empty() {
            vec![]
        } else {
            vec![text_content(output)]
        })),
        details,
        diagnostics: Some(diagnostics),
        ..Default::default()
    })
}
pub fn execute_read(
    args: ReadToolInput,
    options: ReadToolOptions,
    api: Arc<dyn CodingToolApi>,
    context: Context,
) -> StorageFuture<ToolExecutionResult> {
    eager(async move {
        let env = require_env(api.environment()?)?;
        let path =
            resolve_read_tool_path(env.as_ref(), &args.path.to_string_lossy(), &context).await?;
        let reader = env
            .open_binary_reader(&path, false, &context)
            .await
            .map_err(file_error)?;
        let result = async {
            for attempt in 0..2 {
                let before = reader.info(&context).await.map_err(file_error)?;
                let mime =
                    detect_supported_image_mime_type_of(reader.as_ref(), before.size, &context)
                        .await?;
                let result = if let Some(mime) = mime {
                    let model = api.image_model(&context).await?;
                    read_image(
                        reader.as_ref(),
                        &before,
                        &args.path,
                        mime,
                        options.images.as_ref(),
                        model,
                        &context,
                    )
                    .await?
                } else {
                    read_text(reader.as_ref(), &args, &context).await?
                };
                let after = reader.info(&context).await.map_err(file_error)?;
                if after.size == before.size && after.mtime_ms == before.mtime_ms
                    || mime.is_none() && after.size > before.size
                {
                    return Ok(result);
                }
                if attempt == 1 {
                    return Err(Error::message(format!(
                        "{} changed while it was read",
                        args.path
                    )));
                }
            }
            unreachable!()
        }
        .await;
        reader.close(&context).await.map_err(file_error)?;
        result
    })
}
pub fn create_read_tool(options: ReadToolOptions) -> NativeTool {
    let formats = if options.images.is_some() {
        "jpg, png, gif, webp, bmp"
    } else {
        "jpg, png, gif, webp"
    };
    let schema = wire::encode(
        &serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}},"required":["path"]}),
    );
    NativeTool::new(
        ToolDeclaration::new(
            "read",
            format!(
                "Read the contents of a file. Supports text files and images ({formats}). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
            schema,
        ),
        Arc::new(move |args, api, context| match wire::decode(args.value()) {
            Ok(args) => execute_read(args, options.clone(), Arc::new(api), context),
            Err(error) => Box::pin(async { Err(Error::message(error)) }),
        }),
    )
}
