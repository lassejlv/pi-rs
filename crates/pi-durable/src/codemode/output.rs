//! Script result formatting, bounded text spills and deduplicated image files.
use crate::{
    harness::tool::{ToolContent, join, text_content},
    storage::StorageFuture,
    *,
};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use indexmap::IndexMap;
use std::sync::Arc;

pub const DEFAULT_MAX_OUTPUT_TOKENS: f64 = 10_000.0;
#[derive(Clone, Debug)]
pub enum OutputData {
    Text(JsonString),
    Binary(Vec<u8>),
}
#[derive(Clone, Debug)]
pub enum OutputFileResult {
    Saved(JsonString),
    Failed(JsonString),
}
pub type OutputFile =
    Arc<dyn Fn(String, OutputData) -> StorageFuture<OutputFileResult> + Send + Sync>;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CallSummary {
    pub name: JsonString,
    pub status: JsonString,
}
pub struct FormatResultOptions {
    pub generated_images: usize,
    pub wall_ms: f64,
    pub max_output_tokens: f64,
    pub calls: Vec<CallSummary>,
    pub max_bytes: f64,
    pub max_lines: f64,
    pub write: OutputFile,
}
#[derive(Clone, Debug)]
pub struct FormattedResult {
    pub content: ToolContent,
    pub full_output_path: Option<JsonString>,
}
fn text(block: &JsonObject) -> Option<&JsonString> {
    block.value()["type"]
        .string_equals("text")
        .then(|| block.value()["text"].as_string())
        .flatten()
}
fn is_image(block: &JsonObject) -> bool {
    block.value()["type"].string_equals("image")
}
fn append(left: impl Into<JsonString>, right: &JsonString) -> JsonString {
    let mut out = left.into();
    out.append(right);
    out
}
fn format_items(output: &[JsonObject]) -> Result<Vec<Arc<JsonObject>>> {
    let total = output
        .iter()
        .filter(|item| text(item).is_some() && item.value()["console"].as_bool() != Some(true))
        .count();
    let mut items = Vec::new();
    let mut console = Vec::new();
    let mut index = 0;
    for item in output {
        if is_image(item) {
            items.push(Arc::new(item.clone()));
        } else {
            let value: JsonString = wire::field(item.value(), "text").map_err(Error::message)?;
            if item.value()["console"].as_bool() == Some(true) {
                console.push(value);
            } else {
                index += 1;
                items.push(text_content(if total > 1 {
                    append(format!("==> text {index}/{total} <==\n"), &value)
                } else {
                    value
                }));
            }
        }
    }
    if !console.is_empty() {
        items.push(text_content(join(
            [
                "<console_output>\n".into(),
                join(console, "\n"),
                "\n</console_output>".into(),
            ],
            "",
        )));
    }
    Ok(items)
}
pub fn join_adjacent_text(items: Vec<Arc<JsonObject>>) -> Vec<Arc<JsonObject>> {
    let mut joined = Vec::new();
    let mut parts = Vec::new();
    let mut last: Option<bool> = None;
    let flush = |parts: &mut Vec<JsonString>, joined: &mut Vec<Arc<JsonObject>>| {
        if !parts.is_empty() {
            joined.push(text_content(join(std::mem::take(parts), "")));
        }
    };
    for item in items {
        let Some(value) = text(&item) else {
            flush(&mut parts, &mut joined);
            last = None;
            joined.push(item);
            continue;
        };
        if last == Some(false) {
            parts.push("\n".into());
            last = Some(true);
        }
        parts.push(value.clone());
        if !value.units().is_empty() {
            last = Some(value.units().last() == Some(&10));
        }
    }
    flush(&mut parts, &mut joined);
    joined
}
fn slice<T>(values: &[T], begin: f64, end: Option<f64>) -> &[T] {
    let at = |index: f64| {
        if index.is_nan() {
            0
        } else if index < 0.0 {
            (values.len() as f64 + index.trunc()).max(0.0) as usize
        } else {
            index.trunc().min(values.len() as f64) as usize
        }
    };
    let begin = at(begin);
    let end = end.map_or(values.len(), at).max(begin);
    &values[begin..end]
}
fn line_slice(units: &[u16], begin: f64, end: Option<f64>) -> JsonString {
    let lines: Vec<_> = units.split(|&u| u == 10).collect();
    join(
        slice(&lines, begin, end)
            .iter()
            .map(|line| JsonString::from_utf16(line.to_vec())),
        "\n",
    )
}
async fn truncate(
    items: Vec<Arc<JsonObject>>,
    options: &FormatResultOptions,
) -> Result<(Vec<Arc<JsonObject>>, Option<JsonString>)> {
    let texts: Vec<_> = items
        .iter()
        .filter_map(|item| text(item).cloned())
        .collect();
    let combined = join(texts.iter().cloned(), "\n");
    let token_budget = options.max_output_tokens * 4.0;
    let byte_budget = (options.max_bytes / 4.0).floor();
    let budget = if token_budget.is_nan() || byte_budget.is_nan() {
        f64::NAN
    } else {
        token_budget.min(byte_budget)
    };
    let line_budget = ((options.max_lines - 100.0) / 2.0).floor();
    let lines = combined.units().split(|&u| u == 10).count();
    if texts.is_empty()
        || combined.units().len() as f64 <= budget && lines as f64 <= 2.0 * line_budget
    {
        return Ok((items, None));
    }
    let head_chars = (budget / 2.0).floor();
    let tail_chars = budget - head_chars;
    let head = line_slice(
        slice(combined.units(), 0.0, Some(head_chars)),
        0.0,
        Some(line_budget),
    );
    let tail = if tail_chars > 0.0 {
        line_slice(
            slice(combined.units(), -tail_chars, None),
            -line_budget,
            None,
        )
    } else {
        JsonString::default()
    };
    let removed =
        combined.units().len() as f64 - head.units().len() as f64 - tail.units().len() as f64;
    let mut value = join([format!("Warning: truncated output (original token count: {})\nTotal output lines: {lines}\n\n", pi_json::number_to_string((combined.units().len() as f64/4.0).ceil())).into(),head,format!("…{} tokens truncated…",pi_json::number_to_string((removed/4.0).ceil())).into(),tail], "");
    let path = match (options.write)("output.txt".into(), OutputData::Text(combined)).await? {
        OutputFileResult::Saved(path) => {
            value.append(&join(
                [
                    "\n\n[Full output: ".into(),
                    path.clone(),
                    " (read with offset/limit)]".into(),
                ],
                "",
            ));
            Some(path)
        }
        OutputFileResult::Failed(error) => {
            value.append(&join(
                [
                    "\n\n[Could not save the full output: ".into(),
                    error,
                    "]".into(),
                ],
                "",
            ));
            None
        }
    };
    let mut result = vec![text_content(value)];
    result.extend(items.into_iter().filter(|item| is_image(item)));
    Ok((result, path))
}
async fn save_images(
    items: Vec<Arc<JsonObject>>,
    write: &OutputFile,
) -> Result<Vec<Arc<JsonObject>>> {
    let mut labels: IndexMap<JsonString, usize> = IndexMap::new();
    let mut pending: Vec<StorageFuture<JsonString>> = Vec::new();
    for item in &items {
        if !is_image(item) {
            continue;
        }
        let data: JsonString = wire::field(item.value(), "data").map_err(Error::message)?;
        if labels.contains_key(&data) {
            continue;
        }
        let mime: JsonString = wire::field(item.value(), "mimeType").map_err(Error::message)?;
        let input: Vec<_> = data
            .to_string_lossy()
            .bytes()
            .filter(|b| !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
            .collect();
        let bytes = GeneralPurpose::new(
            &alphabet::STANDARD,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true),
        )
        .decode(&input)
        .map_err(|error| Error::message(error.to_string()))?;
        let kind = join(
            [mime.clone(), format!(", {} bytes", bytes.len()).into()],
            "",
        );
        let extension = match mime.to_string_lossy().as_str() {
            "image/png" => ".png",
            "image/jpeg" => ".jpg",
            "image/gif" => ".gif",
            "image/webp" => ".webp",
            _ => ".bin",
        };
        let index = labels.len();
        labels.insert(data, index);
        let operation = write(
            format!("image-{}{extension}", index + 1),
            OutputData::Binary(bytes),
        );
        pending.push(Box::pin(async move {
            Ok(match operation.await? {
                OutputFileResult::Saved(path) => join(
                    [
                        "[Image saved to ".into(),
                        path,
                        " (".into(),
                        kind,
                        ")]".into(),
                    ],
                    "",
                ),
                OutputFileResult::Failed(error) => join(
                    [
                        "[Image (".into(),
                        kind,
                        ") could not be saved: ".into(),
                        error,
                        "]".into(),
                    ],
                    "",
                ),
            })
        }));
    }
    let labels_text = futures::future::join_all(pending)
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for item in items {
        if is_image(&item) {
            let data: JsonString = wire::field(item.value(), "data").map_err(Error::message)?;
            result.push(text_content(labels_text[labels[&data]].clone()));
        }
        result.push(item);
    }
    Ok(result)
}
fn format_error(result: &JsonValue, calls: &[CallSummary]) -> JsonString {
    let error = &result["error"];
    let message = error["message"].as_string().cloned().unwrap_or_default();
    let head = match error["kind"].to_utf8().as_deref() {
        Some("script") => error["stack"].as_string().cloned().unwrap_or_else(|| {
            join(
                [
                    error["name"]
                        .as_string()
                        .cloned()
                        .unwrap_or_else(|| "Error".into()),
                    ": ".into(),
                    message,
                ],
                "",
            )
        }),
        Some("timeout") => append("Script timed out: ", &message),
        Some("aborted") => append("Script aborted: ", &message),
        _ => append("Script sandbox failed: ", &message),
    };
    let summary = if calls.is_empty() {
        "No tool calls were made.".into()
    } else {
        append(
            "Tool calls made before the failure (they are not undone): ",
            &join(
                calls.iter().map(|call| {
                    join(
                        [
                            call.name.clone(),
                            " (".into(),
                            call.status.clone(),
                            ")".into(),
                        ],
                        "",
                    )
                }),
                ", ",
            ),
        )
    };
    join([head, summary], "\n\n")
}
/// Strict VM wire results retain their UTF16 string units throughout formatting.
pub fn format_result(
    result: JsonValue,
    options: FormatResultOptions,
) -> StorageFuture<FormattedResult> {
    Box::pin(async move {
        let ok: bool = wire::field(&result, "ok").map_err(Error::message)?;
        let mut output: Vec<JsonObject> = wire::field(&result, "output").map_err(Error::message)?;
        if ok && let Some(value) = wire::get(&result, "value") {
            let value = value
                .as_string()
                .cloned()
                .unwrap_or_else(|| value.to_string().into());
            output.push((*text_content(value)).clone());
        }
        let mut items = format_items(&output)?;
        if !ok {
            items.push(text_content(append(
                "Script error:\n",
                &format_error(&result, &options.calls),
            )));
        }
        if options.generated_images > 0 && !items.iter().any(|item| is_image(item)) {
            items.push(text_content(format!("Note: models.generateImages() returned {} image{} that the script did not show. Show each image block of result.output with image(block).",options.generated_images,if options.generated_images == 1 {""} else {"s"}).into()));
        }
        let (items, path) = truncate(join_adjacent_text(items), &options).await?;
        let content = join_adjacent_text(save_images(items, &options.write).await?);
        let header = format!(
            "Script {}\nWall time {} seconds\nOutput:\n",
            if ok { "completed" } else { "failed" },
            crate::truncate::to_fixed(options.wall_ms / 1000.0, 1)
        );
        let mut items = vec![text_content(header.into())];
        items.extend(content);
        Ok(FormattedResult {
            content: Arc::new(items),
            full_output_path: path,
        })
    })
}
