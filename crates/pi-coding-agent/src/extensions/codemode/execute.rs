//! Port of `packages/coding-agent/src/extensions/codemode/execute.ts`.
//!
//! Runs one codemode script in the sandbox (`pi-codemode`, a QuickJS VM on its own thread per run).

use crate::core::extensions::types::{ExecuteToolOptions, ExtensionToolContext, ToolNamespace};
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::SessionEntry;
use crate::core::tools::js_util::{utf16_len, utf16_offset_to_byte, utf16_prefix};
use crate::core::tools::truncate::format_size;
use crate::core::usage_totals::combine_usage;
use crate::extensions::codemode::tool::{
    CODEMODE_DOCS_PATH, CODEMODE_STORE_ENTRY_TYPE, CodemodeNestedCall, CodemodeNestedCallStatus, CodemodeStoreEntryData,
    CodemodeToolDetails, CodemodeToolInput, CodemodeToolOptions, get_codemode_callable_tools, to_codemode_declaration,
};
use crate::extensions::tool_search::tool::{
    Bm25Ranker, CreateToolSearchDocumentTool, DEFAULT_TOOL_SEARCH_LIMIT, ToolRanker, create_tool_search_document,
};
use crate::utils::output_files::{WriteOutputFileData, write_output_file};
use base64::Engine;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_agent::types::{AgentToolCallOutcome, AgentToolRef, AgentToolResult, AgentToolUpdateCallback};
use pi_ai::types::{
    AnyModel, ClassifierContext, ClassifierStopReason, ImageContent, ImagesContext, ImagesStopReason, ModelType,
    StreamOptions, UserContentBlock, Usage,
};
use pi_codemode::{
    AcquireWorker, CodemodeError, CodemodeErrorKind, CodemodeExecuteOptions, CodemodeOutputItem, CodemodeResult,
    CodemodeSandbox, CodemodeSandboxOptions, CodemodeTool, CodemodeToolContext, WorkerRelease, parse_codemode_source,
    render_tool_sample, to_codemode_identifier,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// A JSON value from a script, read field by field: values that are not objects have no fields.
pub type ScriptValue = Option<IndexMap<String, Value>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelCallResultStopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

impl ModelCallResultStopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Error => "error",
            Self::Aborted => "aborted",
        }
    }
}

/// The fields of `ClassifierResult` and `AssistantImages` that a nested call row reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCallResult {
    pub stop_reason: ModelCallResultStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;
/// `models.classify()` and `models.generateImages()` calls one script may have in flight; `Promise.all`
/// over many items queues the rest.
const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
/// Heap limit for the VM (QuickJS's heap limit), as in the TS sandbox options.
const CODEMODE_MEMORY_LIMIT_BYTES: u64 = 256 * 1024 * 1024;
/// Shared across calls and sessions. Allow one replacement while a cancelled worker is still exiting.
const DEFAULT_MAX_CODEMODE_WORKERS: usize = 2;

/// `PI_CODEMODE_MAX_WORKERS` if it is a positive integer. Read per call so SDK hosts can set it at runtime.
fn max_codemode_workers() -> usize {
    std::env::var("PI_CODEMODE_MAX_WORKERS")
        .ok()
        .and_then(|value| {
            let number: f64 = if value.trim().is_empty() { 0.0 } else { value.trim().parse().ok()? };
            (number.fract() == 0.0 && number > 0.0 && number <= usize::MAX as f64).then_some(number as usize)
        })
        .unwrap_or(DEFAULT_MAX_CODEMODE_WORKERS)
}

/// How long a script waits for a free worker before failing. Bounds the wait when every worker is
/// held by a script that waits on another script, for example an in-process subagent that uses codemode.
const CODEMODE_WORKER_WAIT_MS: f64 = 60_000.0;

/// Outcome of [`WorkerAdmission::reserve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Reserved,
    Aborted,
    Timeout,
}

/// Worker slots shared by every script in the process, granted in arrival order.
pub(crate) struct WorkerAdmission {
    state: Mutex<AdmissionState>,
    max: Box<dyn Fn() -> usize + Send + Sync>,
}

#[derive(Default)]
struct AdmissionState {
    active: usize,
    next_id: u64,
    /// Scripts waiting for a worker, in arrival order.
    waiters: VecDeque<(u64, tokio::sync::oneshot::Sender<()>)>,
}

impl WorkerAdmission {
    pub(crate) fn new(max: impl Fn() -> usize + Send + Sync + 'static) -> Self {
        Self { state: Mutex::new(AdmissionState::default()), max: Box::new(max) }
    }

    /// Give free slots to waiting scripts in arrival order.
    fn admit(&self, state: &mut AdmissionState) {
        let max = (self.max)();
        while state.active < max {
            let Some((_, grant)) = state.waiters.pop_front() else { break };
            // A waiter that already gave up dropped its receiver; its slot goes to the next one.
            if grant.send(()).is_ok() {
                state.active += 1;
            }
        }
    }

    /// Reserve one worker slot, waiting in arrival order while all are taken. Resolves with why it
    /// could not reserve one: the signal aborted, or `timeout_ms` passed.
    pub(crate) async fn reserve(&self, signal: Option<&CancellationToken>, timeout_ms: f64) -> Admission {
        if signal.is_some_and(|s| s.is_cancelled()) {
            return Admission::Aborted;
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let id = {
            let mut state = self.state.lock();
            let id = state.next_id;
            state.next_id += 1;
            state.waiters.push_back((id, tx));
            self.admit(&mut state);
            id
        };
        let never = CancellationToken::new();
        let cancelled = signal.unwrap_or(&never).cancelled();
        let timeout = async {
            if timeout_ms.is_finite() {
                tokio::time::sleep(Duration::from_millis(timeout_ms.max(0.0) as u64)).await
            } else {
                std::future::pending::<()>().await
            }
        };
        let outcome = tokio::select! {
            biased;
            granted = &mut rx => if granted.is_ok() { Admission::Reserved } else { Admission::Aborted },
            _ = cancelled => Admission::Aborted,
            _ = timeout => Admission::Timeout,
        };
        if outcome == Admission::Reserved {
            return outcome;
        }
        let mut state = self.state.lock();
        if let Some(index) = state.waiters.iter().position(|(waiter, _)| *waiter == id) {
            state.waiters.remove(index);
            return outcome;
        }
        drop(state);
        // Granted while giving up: keep the slot, like a grant that settled first.
        if rx.try_recv().is_ok() { Admission::Reserved } else { outcome }
    }

    pub(crate) fn release(&self) {
        let mut state = self.state.lock();
        state.active = state.active.saturating_sub(1);
        self.admit(&mut state);
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.state.lock().active
    }
}

static CODEMODE_WORKERS: LazyLock<WorkerAdmission> = LazyLock::new(|| WorkerAdmission::new(max_codemode_workers));

const MODEL_TYPES: [&str; 3] = ["chat", "image", "classifier"];

fn model_type_of(name: &str) -> ModelType {
    match name {
        "image" => ModelType::Image,
        "classifier" => ModelType::Classifier,
        _ => ModelType::Chat,
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if utf16_len(text) > max_chars { format!("{}...", utf16_prefix(text, max_chars - 3)) } else { text.to_string() }
}

fn preview_args(args: Option<&Value>) -> String {
    match args {
        None => String::new(),
        Some(args) => truncate_text(&serde_json::to_string(args).unwrap_or_default(), ARGS_PREVIEW_CHARS),
    }
}

fn text_of(result: &AgentToolResult) -> String {
    result.content.iter().filter_map(|block| block.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("\n")
}

/// `JSON.stringify(value)` of an optional value (`undefined` for `None`).
fn json_text(value: Option<&Value>) -> String {
    value.map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_else(|| "undefined".to_string())
}

fn to_model_type(value: Option<&Value>) -> Result<ModelType, String> {
    match value {
        Some(Value::String(name)) if MODEL_TYPES.contains(&name.as_str()) => Ok(model_type_of(name)),
        _ => Err(format!("Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".", json_text(value))),
    }
}

fn to_provider(value: Option<&Value>) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(provider)) => Ok(Some(provider.clone())),
        Some(_) => Err("provider must be a string".to_string()),
    }
}

/// Catalog entry for scripts. `headers` is dropped because models.json headers can carry credentials.
fn to_model_info(model: &AnyModel) -> Value {
    let mut info = serde_json::to_value(model).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut info {
        map.shift_remove("headers");
    }
    info
}

/// `an image`, `a classifier`.
fn with_article(word: &str) -> String {
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    format!("{article} {word}")
}

/// A script value in an error message: `undefined`, `a string`, `an array`, or its keys (`{ prompt }`).
fn describe_value(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Array(items)) => if items.is_empty() { "an empty array" } else { "an array" }.to_string(),
        Some(Value::Object(map)) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            let keys: Vec<&str> = map.keys().take(6).map(String::as_str).collect();
            format!("{{ {}{} }}", keys.join(", "), if map.len() > 6 { ", ..." } else { "" })
        }
        Some(Value::String(_)) => "a string".to_string(),
        Some(Value::Number(_)) => "a number".to_string(),
        Some(Value::Bool(_)) => "a boolean".to_string(),
    }
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";

fn is_plain_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn field<'a>(value: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    value.and_then(Value::as_object).and_then(|map| map.get(name))
}

fn is_image_block(value: Option<&Value>) -> bool {
    field(value, "type").and_then(Value::as_str) == Some("image")
        && field(value, "data").is_some_and(Value::is_string)
        && field(value, "mimeType").is_some_and(Value::is_string)
}

/// Check a script's classifier context, so mistakes fail with the expected shape instead of a provider error.
fn check_classifier_context(context: Option<&Value>) -> Result<ClassifierContext, String> {
    let fail = |problem: String| {
        format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {}.",
            *CODEMODE_DOCS_PATH
        )
    };
    let Some(map) = is_plain_object(context) else {
        return Err(fail(format!("expects a context object as its second argument, got {}", describe_value(context))));
    };
    let state = map.get("state");
    if is_plain_object(state).is_none() {
        return Err(fail(format!("context.state must be an object, got {}", describe_value(state))));
    }
    if let Some(images) = map.get("images") {
        let Value::Array(images) = images else {
            return Err(fail(format!("context.images must be an array, got {}", describe_value(Some(images)))));
        };
        for (index, image) in images.iter().enumerate() {
            if !is_image_block(Some(image)) {
                return Err(fail(format!(
                    "context.images[{index}] must be an image block, got {}",
                    describe_value(Some(image))
                )));
            }
        }
    }
    let questions = map.get("questions");
    let Some(questions_map) = is_plain_object(questions).filter(|q| !q.is_empty()) else {
        return Err(fail(format!(
            "context.questions must map question IDs to questions, got {}",
            describe_value(questions)
        )));
    };
    let is_strings = |values: Vec<&Value>| !values.is_empty() && values.iter().all(|v| v.is_string());
    for (id, question) in questions_map {
        let at = format!("context.questions.{id}");
        let Some(question_map) = question.as_object() else {
            return Err(fail(format!("{at} must be a question object, got {}", describe_value(Some(question)))));
        };
        if !question_map.get("instructions").is_some_and(Value::is_string) {
            return Err(fail(format!("{at}.instructions must be a string")));
        }
        let criteria = question_map.get("criteria");
        match question_map.get("type") {
            Some(Value::String(kind)) if kind == "choice" => {
                if !criteria.and_then(Value::as_object).is_some_and(|c| is_strings(c.values().collect())) {
                    return Err(fail(format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
            }
            Some(Value::String(kind)) if kind == "score" => {
                if !criteria.and_then(Value::as_array).is_some_and(|c| is_strings(c.iter().collect())) {
                    return Err(fail(format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
            }
            Some(Value::String(kind)) if kind == "bool" => {
                if !field(criteria, "true").is_some_and(Value::is_string)
                    || !field(criteria, "false").is_some_and(Value::is_string)
                {
                    return Err(fail(format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                }
            }
            other => {
                return Err(fail(format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got {}",
                    json_text(other)
                )));
            }
        }
    }
    serde_json::from_value(context.cloned().unwrap_or(Value::Null)).map_err(|error| fail(error.to_string()))
}

/// Check a script's image context, so mistakes such as `{ prompt }` fail with the expected shape.
fn check_images_context(context: Option<&Value>) -> Result<ImagesContext, String> {
    let fail = |problem: String| {
        format!(
            "models.generateImages() {problem}. Expected context: {{ input: [{{ type: \"text\", text: <prompt> }}, ...optional {{ type: \"image\", data: <base64>, mimeType }} references] }}. See \"Generate images\" in {}.",
            *CODEMODE_DOCS_PATH
        )
    };
    let Some(map) = is_plain_object(context) else {
        return Err(fail(format!("expects a context object as its second argument, got {}", describe_value(context))));
    };
    let input = map.get("input");
    let Some(blocks) = input.and_then(Value::as_array).filter(|blocks| !blocks.is_empty()) else {
        return Err(fail(format!("context.input must be a non-empty array of blocks, got {}", describe_value(input))));
    };
    for (index, block) in blocks.iter().enumerate() {
        let is_text = field(Some(block), "type").and_then(Value::as_str) == Some("text")
            && field(Some(block), "text").is_some_and(Value::is_string);
        if is_text || is_image_block(Some(block)) {
            continue;
        }
        return Err(fail(format!(
            "context.input[{index}] must be a text or image block, got {}",
            describe_value(Some(block))
        )));
    }
    serde_json::from_value(context.cloned().unwrap_or(Value::Null)).map_err(|error| fail(error.to_string()))
}

fn is_store_entry_data(data: Option<&Value>) -> Option<CodemodeStoreEntryData> {
    let map = data?.as_object()?;
    let set = map.get("set")?.as_object()?;
    let deleted = map.get("delete")?.as_array()?;
    let delete: Option<Vec<String>> = deleted.iter().map(|key| key.as_str().map(str::to_string)).collect();
    Some(CodemodeStoreEntryData { set: set.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), delete: delete? })
}

/// Values of `load()`: the `codemode-store` entries on the branch, applied from the root.
pub fn read_codemode_store(branch: &[SessionEntry]) -> IndexMap<String, Value> {
    let mut store: IndexMap<String, Value> = IndexMap::new();
    for entry in branch {
        let SessionEntry::Custom(entry) = entry else { continue };
        if entry.custom_type != CODEMODE_STORE_ENTRY_TYPE {
            continue;
        }
        let Some(data) = is_store_entry_data(entry.data.as_ref()) else { continue };
        for key in &data.delete {
            store.shift_remove(key);
        }
        for (key, value) in data.set {
            store.insert(key, value);
        }
    }
    store
}

/// Default token budget for script output.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// Characters per token when estimating.
const CHARS_PER_TOKEN: usize = 4;

/// Like the script's `text()`: strings as is, other values as compact JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Lay out the script's output so the model can tell items apart: providers join adjacent text
/// blocks with a newline or with nothing. With more than one text item (`text()` or the returned
/// value), each starts with a `==> text N/M <==` line. `console.*` lines follow all other output in
/// one `<console_output>` block.
fn format_output(output: &[CodemodeOutputItem]) -> Vec<UserContentBlock> {
    let is_console = |console: &Option<bool>| *console == Some(true);
    let total = output
        .iter()
        .filter(|item| matches!(item, CodemodeOutputItem::Text { console, .. } if !is_console(console)))
        .count();
    let mut items = Vec::new();
    let mut console_lines: Vec<&str> = Vec::new();
    let mut index = 0;
    for item in output {
        match item {
            CodemodeOutputItem::Image { data, mime_type } => {
                items.push(UserContentBlock::Image(ImageContent { data: data.clone(), mime_type: mime_type.clone() }))
            }
            CodemodeOutputItem::Text { text, console } if is_console(console) => console_lines.push(text),
            CodemodeOutputItem::Text { text, .. } => {
                index += 1;
                items.push(UserContentBlock::text(if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}")
                } else {
                    text.clone()
                }));
            }
        }
    }
    if !console_lines.is_empty() {
        items.push(UserContentBlock::text(format!("<console_output>\n{}\n</console_output>", console_lines.join("\n"))));
    }
    items
}

/// Join adjacent text items into one, each part starting on its own line.
fn join_adjacent_text(items: Vec<UserContentBlock>) -> Vec<UserContentBlock> {
    let mut joined = Vec::new();
    let mut parts = String::new();
    let mut has_parts = false;
    let mut last_part_ends_line = true;
    let flush = |joined: &mut Vec<UserContentBlock>, parts: &mut String, has_parts: &mut bool| {
        if *has_parts {
            joined.push(UserContentBlock::text(std::mem::take(parts)));
        }
        *has_parts = false;
    };
    for item in items {
        let Some(text) = item.as_text().map(|t| t.text.clone()) else {
            flush(&mut joined, &mut parts, &mut has_parts);
            last_part_ends_line = true;
            joined.push(item);
            continue;
        };
        if !last_part_ends_line {
            parts.push('\n');
            last_part_ends_line = true;
        }
        parts.push_str(&text);
        has_parts = true;
        if !text.is_empty() {
            last_part_ends_line = text.ends_with('\n');
        }
    }
    flush(&mut joined, &mut parts, &mut has_parts);
    joined
}

fn format_call_summary(calls: &[CodemodeNestedCall]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".to_string();
    }
    format!(
        "Tool calls made before the failure (they are not undone): {}",
        calls.iter().map(|call| format!("{} ({})", call.name, call.status.as_str())).collect::<Vec<_>>().join(", ")
    )
}

fn format_error(error: &CodemodeError, calls: &[CodemodeNestedCall]) -> String {
    let head = match error.kind {
        CodemodeErrorKind::Script => error.stack.clone().unwrap_or_else(|| {
            format!("{}: {}", error.name.clone().unwrap_or_else(|| "Error".to_string()), error.message)
        }),
        CodemodeErrorKind::Timeout => format!("Script timed out: {}", error.message),
        CodemodeErrorKind::Aborted => format!("Script aborted: {}", error.message),
        CodemodeErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
    };
    format!("{head}\n\n{}", format_call_summary(calls))
}

/// Write the full text output to a temp file, like bash does for truncated output.
async fn spill_output(text: &str) -> Result<String, String> {
    write_output_file("pi-codemode", ".txt", &WriteOutputFileData::String(text.to_string()))
        .await
        .map_err(|error| error.to_string())
}

/// File extensions of the image types `image()` accepts. Must list every type the sandbox's
/// `image()` detects.
fn image_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

/// Save each image to a temp file and put a text item with its path before it. The model sees the
/// image but has no other way to reach its bytes: scripts cannot write files, and `write` only takes
/// text. Images shown more than once are saved once.
async fn save_images(items: Vec<UserContentBlock>) -> anyhow::Result<Vec<UserContentBlock>> {
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut result = Vec::new();
    for item in items {
        let Some(image) = item.as_image().cloned() else {
            result.push(item);
            continue;
        };
        let label = match labels.get(&image.data) {
            Some(label) => label.clone(),
            None => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap_or_default();
                let kind = format!("{}, {}", image.mime_type, format_size(bytes.len() as i64));
                let Some(extension) = image_extension(&image.mime_type) else {
                    anyhow::bail!("No file extension for image type {}", image.mime_type);
                };
                // A failed write (disk full, unwritable temp dir) must not discard the result of a
                // script whose tool calls already ran, so it becomes part of the label.
                let label = match write_output_file("pi-codemode", extension, &WriteOutputFileData::List(bytes)).await {
                    Ok(path) => format!("[Image saved to {path} ({kind})]"),
                    Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
                };
                labels.insert(image.data.clone(), label.clone());
                label
            }
        };
        result.push(UserContentBlock::text(label));
        result.push(item);
    }
    Ok(result)
}

/// Apply the token budget: when the combined text exceeds it, the text items become one item that
/// keeps the start and end of the text, and images follow it. The full text is written to a temp file.
async fn truncate_output(items: Vec<UserContentBlock>, max_tokens: u64) -> (Vec<UserContentBlock>, Option<String>) {
    let texts: Vec<&str> = items.iter().filter_map(|item| item.as_text().map(|t| t.text.as_str())).collect();
    let combined = texts.join("\n");
    let budget = (max_tokens as usize).saturating_mul(CHARS_PER_TOKEN);
    let length = utf16_len(&combined);
    if texts.is_empty() || length <= budget {
        return (items, None);
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let removed = length - head_chars - tail_chars;
    let head = utf16_prefix(&combined, head_chars);
    let tail = if tail_chars > 0 { &combined[utf16_offset_to_byte(&combined, length - tail_chars)..] } else { "" };
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{head}…{} tokens truncated…{tail}",
        length.div_ceil(CHARS_PER_TOKEN),
        combined.split('\n').count(),
        removed.div_ceil(CHARS_PER_TOKEN),
    );
    let spilled = spill_output(&combined).await;
    match &spilled {
        Ok(path) => text.push_str(&format!("\n\n[Full output: {path} (read with offset/limit)]")),
        Err(error) => text.push_str(&format!("\n\n[Could not save the full output: {error}]")),
    }
    let mut result = vec![UserContentBlock::text(text)];
    result.extend(items.into_iter().filter(|item| item.as_image().is_some()));
    (result, spilled.ok())
}

/// The value a script receives for a nested call: a tool that declares `outputSchema` resolves to its
/// `structuredContent`, also for error results that carry one (such as MCP results with `isError`);
/// any other tool resolves to its text content. Other failures reject with the tool's error text.
fn to_script_value(tool: &AgentToolRef, outcome: &AgentToolCallOutcome) -> Result<Option<Value>, String> {
    let result = &outcome.result;
    if tool.output_schema().is_some()
        && let Some(structured) = &result.structured_content
    {
        return Ok(Some(structured.clone()));
    }
    let text = text_of(result);
    if outcome.is_error {
        return Err(if text.is_empty() { format!("Tool \"{}\" failed", tool.name()) } else { text });
    }
    Ok(Some(Value::String(text)))
}

/// Shared state of one script run: the nested call rows and how to publish them.
struct RunState {
    calls: Mutex<Vec<CodemodeNestedCall>>,
    on_update: Option<AgentToolUpdateCallback>,
    model_usage: Mutex<Option<Usage>>,
    generated_images: AtomicU64,
}

impl RunState {
    fn snapshot(&self) -> CodemodeToolDetails {
        CodemodeToolDetails { calls: self.calls.lock().clone(), full_output_path: None }
    }

    fn publish(&self) {
        if let Some(on_update) = &self.on_update {
            let details = serde_json::to_value(self.snapshot()).ok();
            on_update(AgentToolResult { content: Vec::new(), details, ..Default::default() });
        }
    }

    fn push(&self, call: CodemodeNestedCall) -> usize {
        let mut calls = self.calls.lock();
        calls.push(call);
        calls.len() - 1
    }

    fn update(&self, index: usize, f: impl FnOnce(&mut CodemodeNestedCall)) {
        if let Some(call) = self.calls.lock().get_mut(index) {
            f(call);
        }
    }

    fn add_model_usage(&self, usage: &Usage) {
        let mut slot = self.model_usage.lock();
        *slot = Some(match slot.as_ref() {
            Some(current) => combine_usage(current, usage),
            None => usage.clone(),
        });
    }
}

fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn seconds_text(since: Instant) -> String {
    crate::core::tools::js_util::js_to_fixed(since.elapsed().as_secs_f64(), 1)
}

/// Run one script. Without a session context (a plain Agent or a direct call) scripts cannot call
/// tools, `store()` starts empty, and writes are dropped.
pub async fn execute_codemode(
    tool_call_id: &str,
    input: &CodemodeToolInput,
    signal: Option<CancellationToken>,
    on_update: Option<AgentToolUpdateCallback>,
    ctx: Option<&ExtensionToolContext>,
    options: Option<&CodemodeToolOptions>,
) -> anyhow::Result<AgentToolResult> {
    let default_options = CodemodeToolOptions::default();
    let options = options.unwrap_or(&default_options);
    let started_at = Instant::now();
    let parsed = parse_codemode_source(&input.code).map_err(|error| anyhow::anyhow!(error.message))?;
    let state = Arc::new(RunState {
        calls: Mutex::new(Vec::new()),
        on_update,
        model_usage: Mutex::new(None),
        generated_images: AtomicU64::new(0),
    });
    // Show the script as running before its first nested call, so the row's timer starts.
    state.publish();

    let callable = match ctx {
        Some(ctx) => get_codemode_callable_tools(&ctx.tools()?),
        None => Vec::new(),
    };
    // ALL_TOOLS entries carry the declaration.
    let guidelines = options.get_tool_guidelines.as_ref().map(|get| get());
    let samples: Arc<HashMap<String, String>> = Arc::new(
        callable
            .iter()
            .map(|tool| {
                let declaration = to_codemode_declaration(
                    tool.as_ref(),
                    guidelines.as_ref().and_then(|g| g.get(tool.name())).map(Vec::as_slice),
                );
                (tool.name().to_string(), render_tool_sample(&declaration, None))
            })
            .collect(),
    );
    let sandbox_tools: Vec<CodemodeTool> = callable
        .iter()
        .map(|tool| {
            let name = tool.name().to_string();
            let tool = tool.clone();
            let state = state.clone();
            let ctx = ctx.cloned();
            let tool_call_id = tool_call_id.to_string();
            let execute = move |args: Option<Value>, call_ctx: CodemodeToolContext| {
                let tool = tool.clone();
                let state = state.clone();
                let ctx = ctx.clone();
                let tool_call_id = tool_call_id.clone();
                async move {
                    let index = state.push(CodemodeNestedCall {
                        id: format!("{tool_call_id}/?"),
                        name: tool.name().to_string(),
                        args: preview_args(args.as_ref()),
                        status: CodemodeNestedCallStatus::Running,
                        duration_ms: None,
                        error: None,
                        cost: None,
                    });
                    state.publish();
                    let call_started_at = Instant::now();
                    // Only tools from ctx.tools are callable, so ctx is set here.
                    let Some(ctx) = ctx else { return Err("Tool calls need a session".to_string()) };
                    let call_signal = call_ctx.signal.clone();
                    let outcome = ctx
                        .execute_tool(
                            tool.name(),
                            args.unwrap_or(Value::Null),
                            ExecuteToolOptions { signal: Some(call_signal.clone()), on_update: None },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    state.update(index, |record| {
                        record.id = outcome.tool_call.id.clone();
                        record.duration_ms = Some(elapsed_ms(call_started_at));
                        if outcome.is_error {
                            record.status = if call_signal.is_cancelled() {
                                CodemodeNestedCallStatus::Cancelled
                            } else {
                                CodemodeNestedCallStatus::Error
                            };
                            let text = text_of(&outcome.result);
                            let text = if text.is_empty() { format!("Tool \"{}\" failed", tool.name()) } else { text };
                            record.error = Some(truncate_text(&text, ERROR_PREVIEW_CHARS));
                        } else {
                            record.status = CodemodeNestedCallStatus::Ok;
                        }
                    });
                    state.publish();
                    to_script_value(&tool, &outcome)
                }
            };
            let mut sandbox_tool = CodemodeTool::new(name.clone(), execute);
            sandbox_tool.description = samples.get(&name).cloned();
            sandbox_tool
        })
        .collect();

    // The wait for a worker counts toward the script's deadline.
    let deadline = parsed.options.timeout_ms.map(|ms| ms as f64).unwrap_or(f64::INFINITY);
    let wait_started_at = Instant::now();
    let admission = CODEMODE_WORKERS.reserve(signal.as_ref(), deadline.min(CODEMODE_WORKER_WAIT_MS)).await;
    let waited_ms = elapsed_ms(wait_started_at);
    if admission != Admission::Reserved {
        let waited = seconds_text(started_at);
        let error = if admission == Admission::Aborted {
            // `reason.message` of the signal's default `DOMException` reason.
            "Script aborted: This operation was aborted".to_string()
        } else if deadline <= CODEMODE_WORKER_WAIT_MS {
            format!(
                "Script timed out: Execution timed out after {} ms while waiting for a free codemode worker",
                deadline as u64
            )
        } else {
            format!(
                "No codemode worker became free within {} seconds. All {} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
                (CODEMODE_WORKER_WAIT_MS / 1000.0) as u64,
                max_codemode_workers()
            )
        };
        return Ok(AgentToolResult {
            content: vec![
                UserContentBlock::text(format!("Script failed\nWall time {waited} seconds\nOutput:\n")),
                UserContentBlock::text(format!("Script error:\n{error}\n\nThe script did not start.")),
            ],
            details: serde_json::to_value(state.snapshot()).ok(),
            is_error: Some(true),
            ..Default::default()
        });
    }
    // The reservation covers the one worker this script starts. A launch that resumes after the
    // result settled finds it released and starts no worker.
    let reserved = Arc::new(AtomicBool::new(true));
    let acquire_reserved = reserved.clone();
    let acquire_worker: AcquireWorker = Arc::new(move || {
        if !acquire_reserved.swap(false, Ordering::SeqCst) {
            return Err("The script already settled".to_string());
        }
        // From here the sandbox releases the slot when the worker actually exits.
        let release: WorkerRelease = Box::new(|| CODEMODE_WORKERS.release());
        Ok(release)
    });

    let mut globals = create_discovery_globals(&callable, samples.clone(), options);
    if options.models == Some(true)
        && let Some(ctx) = ctx
    {
        globals.extend(create_model_globals(ctx.model_registry()?, tool_call_id, state.clone()));
    }
    let timeout_ms = (deadline - waited_ms).round().max(1.0);
    let run = async {
        let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
            tools: sandbox_tools,
            globals,
            timeout_ms: Some(timeout_ms),
            memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
            acquire_worker: Some(acquire_worker),
            ..Default::default()
        })
        .map_err(|error| anyhow::anyhow!(error.0))?;
        let store: Map<String, Value> = match ctx {
            Some(ctx) => read_codemode_store(&ctx.session_manager()?.get_branch(None)).into_iter().collect(),
            None => Map::new(),
        };
        let result = sandbox
            .execute(
                &parsed.code,
                CodemodeExecuteOptions { signal: signal.as_ref().map(Into::into), timeout_ms: None, store: Some(store), timezone: None },
            )
            .await;
        sandbox.close().await;
        result.map_err(|error| anyhow::anyhow!(error.0))
    };
    let result: anyhow::Result<CodemodeResult> = run.await;
    if reserved.swap(false, Ordering::SeqCst) {
        CODEMODE_WORKERS.release();
    }
    let result = result?;
    // Calls still marked running were cut off by the script ending, a timeout, or an abort.
    for call in state.calls.lock().iter_mut() {
        if call.status == CodemodeNestedCallStatus::Running {
            call.status = CodemodeNestedCallStatus::Cancelled;
        }
    }

    let mut script_output = result.output.clone();
    if let Ok(success) = &result.outcome {
        let writes = &success.store_writes;
        if (!writes.set.is_empty() || !writes.delete.is_empty())
            && let Some(append_entry) = &options.append_entry
        {
            append_entry(
                CODEMODE_STORE_ENTRY_TYPE.to_string(),
                CodemodeStoreEntryData {
                    set: writes.set.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    delete: writes.delete.clone(),
                },
            );
        }
        // pi extension: a returned value is appended like text().
        if let Some(value) = &success.value {
            script_output.push(CodemodeOutputItem::text(value_text(value)));
        }
    }
    let mut items = format_output(&script_output);
    if let Err(error) = &result.outcome {
        let calls = state.calls.lock().clone();
        items.push(UserContentBlock::text(format!("Script error:\n{}", format_error(error, &calls))));
    }
    let generated = state.generated_images.load(Ordering::SeqCst);
    if generated > 0 && !items.iter().any(|item| item.as_image().is_some()) {
        items.push(UserContentBlock::text(format!(
            "Note: models.generateImages() returned {generated} image{} that the script did not show. Show each image block of result.output with image(block).",
            if generated == 1 { "" } else { "s" }
        )));
    }

    let (truncated, full_output_path) = truncate_output(
        join_adjacent_text(items),
        parsed.options.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    )
    .await;
    // After truncation, which joins the text items and moves images after them, so each path stays
    // next to its image and is never cut.
    let output = join_adjacent_text(save_images(truncated).await?);
    let header = format!(
        "{}\nWall time {} seconds\nOutput:\n",
        if result.ok() { "Script completed" } else { "Script failed" },
        seconds_text(started_at)
    );
    let mut details = state.snapshot();
    details.full_output_path = full_output_path;
    let mut content = vec![UserContentBlock::text(header)];
    content.extend(output);
    Ok(AgentToolResult {
        content,
        details: Some(serde_json::to_value(details)?),
        usage: state.model_usage.lock().clone(),
        is_error: (!result.ok()).then_some(true),
        ..Default::default()
    })
}

/// Whether `query` names the namespace: its name or one of its aliases, as written or as the same
/// script identifier (`dev-radius` and `dev_radius` are both `dev_radius`).
fn is_namespace_name(namespace: &ToolNamespace, query: &str) -> bool {
    let query_id = to_codemode_identifier(query);
    std::iter::once(&namespace.name)
        .chain(namespace.aliases.iter().flatten())
        .any(|name| name == query || to_codemode_identifier(name) == query_id)
}

fn spread_args(args: Option<Value>) -> Vec<Value> {
    match args {
        Some(Value::Array(items)) => items,
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

fn spread_global<F>(name: &str, execute: F) -> CodemodeTool
where
    F: Fn(Vec<Value>, CodemodeToolContext) -> futures::future::BoxFuture<'static, Result<Option<Value>, String>>
        + Send
        + Sync
        + 'static,
{
    let mut global = CodemodeTool::new(name, move |args, ctx| execute(spread_args(args), ctx));
    global.spread = true;
    global
}

/// `searchTools()`, `describeTool()`, and `describeNamespace()`: ranked search and lookup over the
/// script's nested tools and their namespaces.
fn create_discovery_globals(
    tools: &[AgentToolRef],
    samples: Arc<HashMap<String, String>>,
    options: &CodemodeToolOptions,
) -> Vec<CodemodeTool> {
    let tools: Arc<Vec<AgentToolRef>> = Arc::new(tools.to_vec());
    let get_namespace = options.get_tool_namespace.clone();
    let namespace_of = move |name: &str| get_namespace.as_ref().and_then(|get| get(name.to_string()));
    let namespace_of = Arc::new(namespace_of);

    let search_tools = {
        let (tools, samples, namespace_of) = (tools.clone(), samples.clone(), namespace_of.clone());
        spread_global("searchTools", move |args, _ctx| {
            let result = (|| {
                let Some(Value::String(query)) = args.first() else {
                    return Err("searchTools() expects a query string".to_string());
                };
                let search_options = args.get(1);
                let limit = match field(search_options, "limit") {
                    None | Some(Value::Null) => Some(DEFAULT_TOOL_SEARCH_LIMIT as f64),
                    Some(value) => value.as_f64(),
                };
                let limit = match limit {
                    Some(limit) if limit.fract() == 0.0 && limit > 0.0 => limit as i64,
                    _ => return Err("searchTools() limit must be a positive integer".to_string()),
                };
                let namespace = match field(search_options, "namespace") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(namespace)) => Some(namespace.clone()),
                    Some(_) => return Err("searchTools() namespace must be a string".to_string()),
                };
                let documents: Vec<_> = tools
                    .iter()
                    .filter_map(|tool| {
                        let tool_namespace = namespace_of(tool.name());
                        if let Some(namespace) = namespace.as_deref().filter(|n| !n.is_empty())
                            && !tool_namespace.as_ref().is_some_and(|ns| is_namespace_name(ns, namespace))
                        {
                            return None;
                        }
                        Some(create_tool_search_document(
                            &CreateToolSearchDocumentTool::from(tool.as_ref()),
                            tool_namespace.as_ref(),
                        ))
                    })
                    .collect();
                let matches = Bm25Ranker::new(None).rank(query, &documents, limit);
                Ok(Some(Value::Array(
                    matches
                        .into_iter()
                        .map(|m| {
                            json!({
                                "name": to_codemode_identifier(&m.name),
                                "description": samples.get(&m.name).cloned().unwrap_or_default(),
                            })
                        })
                        .collect(),
                )))
            })();
            Box::pin(async move { result })
        })
    };

    let describe_tool = {
        let (tools, samples) = (tools.clone(), samples.clone());
        spread_global("describeTool", move |args, _ctx| {
            let result = match args.first() {
                Some(Value::String(name)) => Ok(tools
                    .iter()
                    .find(|candidate| candidate.name() == name || to_codemode_identifier(candidate.name()) == *name)
                    .and_then(|tool| samples.get(tool.name()).cloned())
                    .map(Value::String)),
                _ => Err("describeTool() expects a tool name".to_string()),
            };
            Box::pin(async move { result })
        })
    };

    let describe_namespace = {
        let (tools, namespace_of) = (tools.clone(), namespace_of.clone());
        spread_global("describeNamespace", move |args, _ctx| {
            let result = match args.first() {
                Some(Value::String(name)) => {
                    let mut namespace: Option<ToolNamespace> = None;
                    let mut names: Vec<String> = Vec::new();
                    for tool in tools.iter() {
                        let Some(tool_namespace) = namespace_of(tool.name()) else { continue };
                        if !is_namespace_name(&tool_namespace, name) {
                            continue;
                        }
                        namespace.get_or_insert(tool_namespace);
                        names.push(to_codemode_identifier(tool.name()));
                    }
                    Ok(namespace.map(|namespace| {
                        let mut out = Map::new();
                        out.insert("name".into(), Value::String(namespace.name));
                        if let Some(description) = namespace.description.filter(|d| !d.is_empty()) {
                            out.insert("description".into(), Value::String(description));
                        }
                        if let Some(instructions) = namespace.instructions.filter(|i| !i.is_empty()) {
                            out.insert("instructions".into(), Value::String(instructions));
                        }
                        out.insert("tools".into(), json!(names));
                        Value::Object(out)
                    }))
                }
                _ => Err("describeNamespace() expects a namespace name".to_string()),
            };
            Box::pin(async move { result })
        })
    };

    vec![search_tools, describe_tool, describe_namespace]
}

/// Shared state of the `models.*` globals of one script.
struct ModelCalls {
    models: Arc<ModelRegistry>,
    tool_call_id: String,
    state: Arc<RunState>,
    limit: Semaphore,
    call_count: AtomicU64,
}

/// What a model call reports for its row.
struct ModelCallReport {
    stop_reason: ModelCallResultStopReason,
    error_message: Option<String>,
    usage: Option<Usage>,
    value: Value,
}

impl ModelCalls {
    /// Resolve the script's model by provider and id only, check the context, then run the call as
    /// a nested call row. A script-supplied baseUrl or headers must never receive the credentials.
    async fn run<C>(
        self: Arc<Self>,
        name: &str,
        model_type: &str,
        args: Vec<Value>,
        check_context: impl FnOnce(Option<&Value>) -> Result<C, String>,
        run: impl AsyncFnOnce(Arc<ModelRegistry>, AnyModel, C) -> ModelCallReport,
    ) -> Result<Option<Value>, String> {
        let list_hint =
            format!("List the {model_type} models you can use with models.getAvailableOfType(\"{model_type}\").");
        let model = args.first();
        let provider = field(model, "provider").and_then(Value::as_str);
        let id = field(model, "id").and_then(Value::as_str);
        let (Some(provider), Some(id)) = (provider, id) else {
            // undefined arrives as null: spread arguments cross the sandbox as a JSON array.
            let undefined_hint = if matches!(model, None | Some(Value::Null)) {
                " models.getModelOfType() returns undefined for an unknown provider or id."
            } else {
                ""
            };
            return Err(format!(
                "{name}() expects {} model as its first argument, got {}.{undefined_hint} {list_hint}",
                with_article(model_type),
                describe_value(model)
            ));
        };
        let reference = format!("{provider}/{id}");
        let Some(resolved) = self.models.get_model_of_type(model_type_of(model_type), provider, id) else {
            let actual = MODEL_TYPES.iter().find(|other| {
                **other != model_type && self.models.get_model_of_type(model_type_of(other), provider, id).is_some()
            });
            return Err(match actual {
                Some(actual) => format!(
                    "\"{reference}\" is {} model, not {} model. {list_hint}",
                    with_article(actual),
                    with_article(model_type)
                ),
                None => format!("Unknown {model_type} model \"{reference}\". {list_hint}"),
            });
        };
        let checked = check_context(args.get(1))?;

        let count = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
        let index = self.state.push(CodemodeNestedCall {
            id: format!("{}/{name}/{count}", self.tool_call_id),
            name: name.to_string(),
            args: format!("{}/{}", resolved.provider(), resolved.id()),
            status: CodemodeNestedCallStatus::Running,
            duration_ms: None,
            error: None,
            cost: None,
        });
        self.state.publish();
        let started_at = Instant::now();
        let report = {
            let _permit = self.limit.acquire().await.map_err(|error| error.to_string())?;
            run(self.models.clone(), resolved, checked).await
        };
        self.state.update(index, |record| {
            record.duration_ms = Some(elapsed_ms(started_at));
            record.status = match report.stop_reason {
                ModelCallResultStopReason::Stop => CodemodeNestedCallStatus::Ok,
                ModelCallResultStopReason::Aborted => CodemodeNestedCallStatus::Cancelled,
                ModelCallResultStopReason::Error => CodemodeNestedCallStatus::Error,
            };
            if let Some(error) = report.error_message.as_deref().filter(|e| !e.is_empty()) {
                record.error = Some(truncate_text(error, ERROR_PREVIEW_CHARS));
            }
            if let Some(usage) = &report.usage {
                record.cost = Some(usage.cost.total);
            }
        });
        if let Some(usage) = &report.usage {
            self.state.add_model_usage(usage);
        }
        self.state.publish();
        Ok(Some(report.value))
    }
}

fn request_options(signal: &CancellationToken) -> pi_ai::models::ModelsRequestOptions {
    StreamOptions { signal: Some(signal.clone()), ..Default::default() }.into()
}

/// `models.*` for scripts: the model registry methods documented in docs/codemode.md.
/// Classifier and image calls appear as nested call rows so the renderer shows them, and their usage
/// goes to the script's usage. Rows show only the model, never prompts or image data.
fn create_model_globals(models: Arc<ModelRegistry>, tool_call_id: &str, state: Arc<RunState>) -> Vec<CodemodeTool> {
    let calls = Arc::new(ModelCalls {
        models: models.clone(),
        tool_call_id: tool_call_id.to_string(),
        state,
        limit: Semaphore::new(MAX_CONCURRENT_MODEL_CALLS),
        call_count: AtomicU64::new(0),
    });

    let get_models_of_type = {
        let models = models.clone();
        spread_global("models.getModelsOfType", move |args, _ctx| {
            let result = (|| {
                let model_type = to_model_type(args.first())?;
                let provider = to_provider(args.get(1))?;
                Ok(Some(Value::Array(
                    models.get_models_of_type(model_type, provider.as_deref()).iter().map(to_model_info).collect(),
                )))
            })();
            Box::pin(async move { result })
        })
    };

    let get_available_of_type = {
        let models = models.clone();
        spread_global("models.getAvailableOfType", move |args, ctx| {
            let models = models.clone();
            Box::pin(async move {
                let model_type = to_model_type(args.first())?;
                let provider = to_provider(args.get(1))?;
                let available = models
                    .get_available_of_type(
                        model_type,
                        provider.as_deref(),
                        Some(&crate::compat::AuthOperationOptions { signal: Some(ctx.signal.clone()) }),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Some(Value::Array(available.iter().map(to_model_info).collect())))
            })
        })
    };

    let get_model_of_type = {
        let models = models.clone();
        spread_global("models.getModelOfType", move |args, _ctx| {
            let result = (|| {
                let (Some(Value::String(provider)), Some(Value::String(id))) = (args.get(1), args.get(2)) else {
                    return Err(format!(
                        "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
                        args.iter().map(|arg| describe_value(Some(arg))).collect::<Vec<_>>().join(", ")
                    ));
                };
                let model_type = to_model_type(args.first())?;
                Ok(models.get_model_of_type(model_type, provider, id).as_ref().map(to_model_info))
            })();
            Box::pin(async move { result })
        })
    };

    let classify = {
        let calls = calls.clone();
        spread_global("models.classify", move |args, ctx| {
            let calls = calls.clone();
            Box::pin(async move {
                calls
                    .run(
                        "models.classify",
                        "classifier",
                        args,
                        check_classifier_context,
                        async move |models: Arc<ModelRegistry>, resolved: AnyModel, context: ClassifierContext| {
                            let AnyModel::Classifier(model) = resolved else { unreachable!("resolved by type") };
                            let result = models.classify(model, context, Some(request_options(&ctx.signal))).await;
                            ModelCallReport {
                                stop_reason: match result.stop_reason {
                                    ClassifierStopReason::Stop => ModelCallResultStopReason::Stop,
                                    ClassifierStopReason::Error => ModelCallResultStopReason::Error,
                                    ClassifierStopReason::Aborted => ModelCallResultStopReason::Aborted,
                                },
                                error_message: result.error_message.clone(),
                                usage: result.usage.clone(),
                                value: serde_json::to_value(&result).unwrap_or(Value::Null),
                            }
                        },
                    )
                    .await
            })
        })
    };

    let generate_images = {
        let calls = calls.clone();
        spread_global("models.generateImages", move |args, ctx| {
            let calls = calls.clone();
            let state = calls.state.clone();
            Box::pin(async move {
                calls
                    .run(
                        "models.generateImages",
                        "image",
                        args,
                        check_images_context,
                        async move |models: Arc<ModelRegistry>, resolved: AnyModel, context: ImagesContext| {
                            let AnyModel::Image(model) = resolved else { unreachable!("resolved by type") };
                            let result = models.generate_images(model, context, Some(request_options(&ctx.signal))).await;
                            let images = result.output.iter().filter(|block| block.as_image().is_some()).count();
                            state.generated_images.fetch_add(images as u64, Ordering::SeqCst);
                            ModelCallReport {
                                stop_reason: match result.stop_reason {
                                    ImagesStopReason::Stop => ModelCallResultStopReason::Stop,
                                    ImagesStopReason::Error => ModelCallResultStopReason::Error,
                                    ImagesStopReason::Aborted => ModelCallResultStopReason::Aborted,
                                },
                                error_message: result.error_message.clone(),
                                usage: result.usage.clone(),
                                value: serde_json::to_value(&result).unwrap_or(Value::Null),
                            }
                        },
                    )
                    .await
            })
        })
    };

    vec![get_models_of_type, get_available_of_type, get_model_of_type, classify, generate_images]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn admission_queues_in_arrival_order_and_releases_on_exit() {
        let max = Arc::new(AtomicUsize::new(2));
        let limit = max.clone();
        let admission = Arc::new(WorkerAdmission::new(move || limit.load(Ordering::SeqCst)));
        assert_eq!(admission.reserve(None, f64::INFINITY).await, Admission::Reserved);
        assert_eq!(admission.reserve(None, f64::INFINITY).await, Admission::Reserved);
        assert_eq!(admission.active(), 2);

        // At capacity, a script waits for a worker instead of failing.
        let waiting = tokio::spawn({
            let admission = admission.clone();
            async move { admission.reserve(None, f64::INFINITY).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());

        let abort = CancellationToken::new();
        let aborted = tokio::spawn({
            let (admission, abort) = (admission.clone(), abort.clone());
            async move { admission.reserve(Some(&abort), f64::INFINITY).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        abort.cancel();
        assert_eq!(aborted.await.unwrap(), Admission::Aborted);
        assert_eq!(admission.reserve(None, 30.0).await, Admission::Timeout);
        assert!(!waiting.is_finished());

        // Releasing one worker admits exactly the waiting script.
        admission.release();
        assert_eq!(waiting.await.unwrap(), Admission::Reserved);
        assert_eq!(admission.active(), 2);

        // A raised limit admits more.
        max.store(3, Ordering::SeqCst);
        assert_eq!(admission.reserve(None, 50.0).await, Admission::Reserved);
        assert_eq!(admission.active(), 3);
    }

    #[test]
    fn invalid_max_workers_falls_back_to_default() {
        // SAFETY: tests in this module are the only readers of this variable.
        unsafe { std::env::set_var("PI_CODEMODE_MAX_WORKERS", "0") };
        assert_eq!(max_codemode_workers(), DEFAULT_MAX_CODEMODE_WORKERS);
        unsafe { std::env::set_var("PI_CODEMODE_MAX_WORKERS", "3") };
        assert_eq!(max_codemode_workers(), 3);
        unsafe { std::env::remove_var("PI_CODEMODE_MAX_WORKERS") };
        assert_eq!(max_codemode_workers(), DEFAULT_MAX_CODEMODE_WORKERS);
    }

    #[test]
    fn joins_adjacent_text_on_new_lines() {
        let items = vec![
            UserContentBlock::text("a"),
            UserContentBlock::text("b\n"),
            UserContentBlock::text(""),
            UserContentBlock::text("c"),
        ];
        let joined = join_adjacent_text(items);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].as_text().unwrap().text, "a\nb\nc");
    }

    #[test]
    fn formats_output_items() {
        let output = vec![
            CodemodeOutputItem::text("one"),
            CodemodeOutputItem::Text { text: "log".into(), console: Some(true) },
            CodemodeOutputItem::text("two"),
        ];
        let items: Vec<String> = format_output(&output).iter().map(|i| i.as_text().unwrap().text.clone()).collect();
        assert_eq!(
            items,
            vec!["==> text 1/2 <==\none", "==> text 2/2 <==\ntwo", "<console_output>\nlog\n</console_output>"]
        );
    }

    #[test]
    fn describes_script_values() {
        assert_eq!(describe_value(None), "undefined");
        assert_eq!(describe_value(Some(&json!({ "prompt": "x" }))), "{ prompt }");
        assert_eq!(describe_value(Some(&json!([]))), "an empty array");
        assert_eq!(describe_value(Some(&json!(1))), "a number");
        let error = check_images_context(Some(&json!({ "prompt": "cat" }))).unwrap_err();
        assert!(error.starts_with("models.generateImages() context.input must be a non-empty array of blocks, got undefined."));
        let error = check_classifier_context(Some(&json!({ "state": {}, "questions": { "q": { "type": "bool", "instructions": "i", "criteria": {} } } })))
            .unwrap_err();
        assert!(error.contains("context.questions.q is a \"bool\" question"), "{error}");
    }

    #[tokio::test]
    async fn truncates_output_to_the_token_budget() {
        let (items, path) = truncate_output(vec![UserContentBlock::text("x".repeat(100))], 10).await;
        let text = &items[0].as_text().unwrap().text;
        assert!(text.starts_with("Warning: truncated output (original token count: 25)\nTotal output lines: 1\n\n"));
        assert!(text.contains("…15 tokens truncated…"));
        let path = path.expect("spilled");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x".repeat(100));
        let _ = std::fs::remove_file(path);
    }
}
