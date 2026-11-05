//! Faux (scripted) provider for tests (port of `providers/faux.ts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use rand::Rng;
use serde_json::Map;
use tokio_util::sync::CancellationToken;

use crate::auth::types::{ApiKeyAuth, ApiKeyResolveInput, AuthResult, ProviderAuth};
use crate::models::{CreateProviderOptions, Provider, ProviderApi, ProviderStreams, create_provider};
use crate::types::{
    AnyModel, AssistantContent, AssistantMessage, AssistantMessageEvent, DeferredHandle, JsonObject, Message, Model,
    ModelCost, ModelInputLimits, ModelInputModality, ProviderResponse, StopReason, StreamOptions, TextContent,
    ThinkingContent, ToolCall, ToolResultMessage, TranscriptContext, UserContent, UserContentBlock, Usage,
};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::js_len;
use crate::utils::text::get_system_message_text;

const DEFAULT_API: &str = "faux";
const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";
const DEFAULT_MODEL_NAME: &str = "Faux Model";
const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

/// A model definition for the faux provider.
#[derive(Debug, Clone, Default)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<ModelInputModality>>,
    pub input_limits: Option<ModelInputLimits>,
    pub cost: Option<ModelCost>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

pub fn faux_text(text: impl Into<String>) -> AssistantContent {
    AssistantContent::Text(TextContent::new(text))
}

pub fn faux_thinking(thinking: impl Into<String>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent::new(thinking))
}

pub fn faux_tool_call(name: impl Into<String>, arguments: JsonObject, id: Option<&str>) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.map(str::to_string).unwrap_or_else(|| random_id("tool")),
        name: name.into(),
        arguments,
        ..Default::default()
    })
}

/// Options of [`faux_assistant_message`].
#[derive(Debug, Clone, Default)]
pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub response_id: Option<String>,
    pub timestamp: Option<i64>,
}

/// Build a scripted assistant message.
pub fn faux_assistant_message(content: Vec<AssistantContent>, options: FauxMessageOptions) -> AssistantMessage {
    AssistantMessage {
        content,
        api: DEFAULT_API.to_string(),
        provider: DEFAULT_PROVIDER.to_string(),
        model: DEFAULT_MODEL_ID.to_string(),
        usage: Usage::default(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        deferred: options.deferred,
        error_message: options.error_message,
        response_id: options.response_id,
        timestamp: options.timestamp.unwrap_or_else(crate::utils::now_ms),
        ..Default::default()
    }
}

/// A scripted text-only assistant message.
pub fn faux_text_message(text: impl Into<String>) -> AssistantMessage {
    faux_assistant_message(vec![faux_text(text)], FauxMessageOptions::default())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FauxProviderState {
    pub call_count: u64,
    pub deferred_fetch_count: u64,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

pub type FauxResponseFactory = Arc<
    dyn Fn(&TranscriptContext, &StreamOptions, &FauxProviderState, &Model) -> BoxFuture<'static, anyhow::Result<AssistantMessage>>
        + Send
        + Sync,
>;

/// A queued response: a message or a factory computing one from the request.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum FauxResponseStep {
    Message(AssistantMessage),
    Factory(FauxResponseFactory),
}

impl From<AssistantMessage> for FauxResponseStep {
    fn from(value: AssistantMessage) -> Self {
        FauxResponseStep::Message(value)
    }
}

impl FauxResponseStep {
    /// A factory from a synchronous closure.
    pub fn factory(
        f: impl Fn(&TranscriptContext, &StreamOptions, &FauxProviderState, &Model) -> AssistantMessage + Send + Sync + 'static,
    ) -> Self {
        FauxResponseStep::Factory(Arc::new(move |c, o, s, m| {
            let message = f(c, o, s, m);
            Box::pin(async move { Ok(message) })
        }))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FauxDeferredOptions {
    /// Number of fetches that return the original handle before the response becomes ready.
    pub pending_fetches: Option<u64>,
    pub poll_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterFauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Vec<FauxModelDefinition>,
    pub deferred: Option<FauxDeferredOptions>,
    pub tokens_per_second: Option<f64>,
    pub token_size_min: Option<usize>,
    pub token_size_max: Option<usize>,
}

fn random_id(prefix: &str) -> String {
    let mut rng = rand::thread_rng();
    let suffix: String = (0..11)
        .map(|_| {
            let d: u32 = rng.gen_range(0..36);
            std::char::from_digit(d, 36).unwrap_or('0')
        })
        .collect();
    format!("{prefix}:{}:{suffix}", crate::utils::now_ms())
}

fn estimate_tokens(text: &str) -> u64 {
    js_len(text).div_ceil(4) as u64
}

fn content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(t) => t.clone(),
        UserContent::Blocks(blocks) => blocks_to_text(blocks),
    }
}

fn blocks_to_text(blocks: &[UserContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            UserContentBlock::Text(t) => t.text.clone(),
            UserContentBlock::Image(i) => format!("[image:{}:{}]", i.mime_type, js_len(&i.data)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_content_to_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(t) => t.text.clone(),
            AssistantContent::Thinking(t) => t.thinking.clone(),
            AssistantContent::ToolCall(c) => {
                format!("{}:{}", c.name, serde_json::to_string(&c.arguments).unwrap_or_default())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_to_text(message: &ToolResultMessage) -> String {
    let mut parts = vec![message.tool_name.clone()];
    parts.extend(message.content.iter().map(|b| blocks_to_text(std::slice::from_ref(b))));
    parts.join("\n")
}

fn message_to_text(message: &Message) -> String {
    match message {
        Message::System(system) => {
            let mut parts = vec![get_system_message_text(system)];
            for tool in system.tools_removed.iter().flatten() {
                parts.push(format!("tool-:{}", serde_json::to_string(tool).unwrap_or_default()));
            }
            for tool in system.tools_added.iter().flatten() {
                parts.push(format!("tool+:{}", serde_json::to_string(tool).unwrap_or_default()));
            }
            parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n")
        }
        Message::User(user) => content_to_text(&user.content),
        Message::Assistant(assistant) => assistant_content_to_text(&assistant.content),
        Message::ToolResult(result) => tool_result_to_text(result),
    }
}

fn joined_length(messages: &[Vec<u16>], count: usize) -> usize {
    let mut length = if count > 0 { (count - 1) * 2 } else { 0 };
    for message in &messages[..count] {
        length += message.len();
    }
    length
}

fn common_prompt_prefix_length(previous: &[Vec<u16>], current: &[Vec<u16>]) -> usize {
    let mut index = 0;
    while index < previous.len() && index < current.len() && previous[index] == current[index] {
        index += 1;
    }
    let rest = |messages: &[Vec<u16>]| -> Vec<u16> {
        if index == messages.len() {
            return Vec::new();
        }
        let mut out: Vec<u16> = Vec::new();
        if index > 0 {
            out.extend("\n\n".encode_utf16());
        }
        for (i, m) in messages[index..].iter().enumerate() {
            if i > 0 {
                out.extend("\n\n".encode_utf16());
            }
            out.extend(m);
        }
        out
    };
    let a = rest(previous);
    let b = rest(current);
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    joined_length(previous, index) + common
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    context: &TranscriptContext,
    options: &StreamOptions,
    prompt_cache: &Mutex<HashMap<String, Vec<Vec<u16>>>>,
) -> AssistantMessage {
    let prompt: Vec<Vec<u16>> = context
        .messages
        .iter()
        .map(|m| format!("{}:{}", m.role(), message_to_text(m)).encode_utf16().collect())
        .collect();
    let prompt_length = joined_length(&prompt, prompt.len());
    let prompt_tokens = prompt_length.div_ceil(4) as u64;
    let output_tokens = estimate_tokens(&assistant_content_to_text(&message.content));
    let mut input = prompt_tokens;
    let mut cache_read = 0;
    let mut cache_write = 0;
    if let Some(session_id) = options.session_id.as_ref().filter(|s| !s.is_empty())
        && options.cache_retention != Some(crate::types::CacheRetention::None) {
            let mut cache = prompt_cache.lock();
            match cache.get(session_id) {
                Some(previous) => {
                    let cached_chars = common_prompt_prefix_length(previous, &prompt);
                    cache_read = cached_chars.div_ceil(4) as u64;
                    cache_write = (prompt_length - cached_chars).div_ceil(4) as u64;
                    input = prompt_tokens.saturating_sub(cache_read);
                }
                None => cache_write = prompt_tokens,
            }
            cache.insert(session_id.clone(), prompt);
        }
    message.usage = Usage {
        input,
        output: output_tokens,
        cache_read,
        cache_write,
        total_tokens: input + output_tokens + cache_read + cache_write,
        ..Default::default()
    };
    message
}

fn split_string_by_token_size(text: &str, min: usize, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut index = 0;
    let mut rng = rand::thread_rng();
    while index < chars.len() {
        let token_size = min + rng.gen_range(0..=(max - min));
        let char_size = (token_size * 4).max(1);
        let end = (index + char_size).min(chars.len());
        chunks.push(chars[index..end].iter().collect());
        index += char_size;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn create_deferred_message(model: &Model, handle: DeferredHandle) -> AssistantMessage {
    AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Deferred,
        deferred: Some(handle),
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    }
}

fn create_error_message(error: &str, api: &str, provider: &str, model_id: &str) -> AssistantMessage {
    AssistantMessage {
        api: api.to_string(),
        provider: provider.to_string(),
        model: model_id.to_string(),
        stop_reason: StopReason::Error,
        error_message: Some(error.to_string()),
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    }
}

fn create_aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        stop_reason: StopReason::Aborted,
        error_message: Some("Request was aborted".to_string()),
        timestamp: crate::utils::now_ms(),
        ..partial.clone()
    }
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second.filter(|t| *t > 0.0) {
        None => tokio::task::yield_now().await,
        Some(tps) => {
            let delay_ms = (estimate_tokens(chunk) as f64 / tps) * 1000.0;
            crate::utils::wasm::sleep(Duration::from_secs_f64(delay_ms / 1000.0)).await;
        }
    }
}

fn is_aborted(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(|s| s.is_cancelled())
}

fn abort(stream: &AssistantMessageEventSender, partial: &AssistantMessage) {
    let aborted = create_aborted_message(partial);
    stream.push(AssistantMessageEvent::Error { reason: StopReason::Aborted, error: aborted.clone() });
    stream.end(Some(aborted));
}

async fn stream_with_deltas(
    stream: &AssistantMessageEventSender,
    message: AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    signal: Option<CancellationToken>,
) -> anyhow::Result<()> {
    let mut partial = AssistantMessage { content: Vec::new(), stop_reason: StopReason::Pending, ..message.clone() };
    if is_aborted(&signal) {
        abort(stream, &partial);
        return Ok(());
    }
    stream.push(AssistantMessageEvent::Start { partial: partial.clone() });

    for (index, block) in message.content.iter().enumerate() {
        if is_aborted(&signal) {
            abort(stream, &partial);
            return Ok(());
        }
        match block {
            AssistantContent::Thinking(thinking) => {
                partial.content.push(AssistantContent::Thinking(ThinkingContent::new("")));
                stream.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: partial.clone() });
                for chunk in split_string_by_token_size(&thinking.thinking, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if is_aborted(&signal) {
                        abort(stream, &partial);
                        return Ok(());
                    }
                    if let Some(AssistantContent::Thinking(t)) = partial.content.get_mut(index) {
                        t.thinking.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.thinking.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::Text(text) => {
                partial.content.push(AssistantContent::Text(TextContent::new("")));
                stream.push(AssistantMessageEvent::TextStart { content_index: index, partial: partial.clone() });
                for chunk in split_string_by_token_size(&text.text, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if is_aborted(&signal) {
                        abort(stream, &partial);
                        return Ok(());
                    }
                    if let Some(AssistantContent::Text(t)) = partial.content.get_mut(index) {
                        t.text.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::TextDelta { content_index: index, delta: chunk, partial: partial.clone() });
                }
                stream.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.text.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::ToolCall(call) => {
                partial.content.push(AssistantContent::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: Map::new(),
                    ..Default::default()
                }));
                stream.push(AssistantMessageEvent::ToolCallStart { content_index: index, partial: partial.clone() });
                let json = serde_json::to_string(&call.arguments).unwrap_or_default();
                for chunk in split_string_by_token_size(&json, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    if is_aborted(&signal) {
                        abort(stream, &partial);
                        return Ok(());
                    }
                    stream.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                if let Some(AssistantContent::ToolCall(c)) = partial.content.get_mut(index) {
                    c.arguments = call.arguments.clone();
                }
                stream.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: call.clone(),
                    partial: partial.clone(),
                });
            }
        }
    }

    match message.stop_reason {
        StopReason::Pending => anyhow::bail!("Faux response ended without a stop reason"),
        StopReason::Error | StopReason::Aborted => {
            stream.push(AssistantMessageEvent::Error { reason: message.stop_reason, error: message.clone() });
            stream.end(Some(message));
        }
        reason => {
            stream.push(AssistantMessageEvent::Done { reason, message: message.clone() });
            stream.end(Some(message));
        }
    }
    Ok(())
}

struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: TranscriptContext,
    options: StreamOptions,
    model: Model,
    pending_fetches: u64,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

/// The shared core of a faux provider: scripted responses plus streaming.
pub struct FauxCore {
    pub api: String,
    pub provider: String,
    pub models: Vec<Model>,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    deferred_options: Option<FauxDeferredOptions>,
    pending: Mutex<std::collections::VecDeque<FauxResponseStep>>,
    state: Mutex<FauxProviderState>,
    prompt_cache: Mutex<HashMap<String, Vec<Vec<u16>>>>,
    deferred: Mutex<HashMap<String, DeferredEntry>>,
}

impl FauxCore {
    pub fn new(options: &RegisterFauxProviderOptions) -> Arc<Self> {
        let api = options.api.clone().unwrap_or_else(|| random_id(DEFAULT_API));
        let provider = options.provider.clone().unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
        let min_token_size = options
            .token_size_min
            .unwrap_or(DEFAULT_MIN_TOKEN_SIZE)
            .min(options.token_size_max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE))
            .max(1);
        let max_token_size = options.token_size_max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE).max(min_token_size);
        let definitions = if options.models.is_empty() {
            vec![FauxModelDefinition {
                id: DEFAULT_MODEL_ID.to_string(),
                name: Some(DEFAULT_MODEL_NAME.to_string()),
                reasoning: Some(false),
                input: Some(vec![ModelInputModality::Text, ModelInputModality::Image]),
                cost: Some(ModelCost::default()),
                context_window: Some(128000),
                max_tokens: Some(16384),
                ..Default::default()
            }]
        } else {
            options.models.clone()
        };
        let models = definitions
            .into_iter()
            .map(|d| Model {
                name: d.name.clone().unwrap_or_else(|| d.id.clone()),
                id: d.id,
                api: api.clone(),
                provider: provider.clone(),
                base_url: DEFAULT_BASE_URL.to_string(),
                reasoning: d.reasoning.unwrap_or(false),
                input: d.input.unwrap_or_else(|| vec![ModelInputModality::Text, ModelInputModality::Image]),
                input_limits: d.input_limits,
                cost: d.cost.unwrap_or_default(),
                context_window: d.context_window.unwrap_or(128000),
                max_tokens: d.max_tokens.unwrap_or(16384),
                ..Default::default()
            })
            .collect();
        Arc::new(Self {
            api,
            provider,
            models,
            min_token_size,
            max_token_size,
            tokens_per_second: options.tokens_per_second,
            deferred_options: options.deferred,
            pending: Mutex::new(Default::default()),
            state: Mutex::new(FauxProviderState::default()),
            prompt_cache: Mutex::new(HashMap::new()),
            deferred: Mutex::new(HashMap::new()),
        })
    }

    /// The first model, or the one with `model_id`.
    pub fn get_model(&self, model_id: Option<&str>) -> Option<Model> {
        match model_id.filter(|m| !m.is_empty()) {
            None => self.models.first().cloned(),
            Some(id) => self.models.iter().find(|m| m.id == id).cloned(),
        }
    }

    pub fn state(&self) -> FauxProviderState {
        self.state.lock().clone()
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        *self.pending.lock() = responses.into();
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.pending.lock().extend(responses);
    }

    pub fn get_pending_response_count(&self) -> usize {
        self.pending.lock().len()
    }

    async fn resolve_response(
        &self,
        step: &FauxResponseStep,
        context: &TranscriptContext,
        options: &StreamOptions,
        model: &Model,
    ) -> anyhow::Result<AssistantMessage> {
        let resolved = match step {
            FauxResponseStep::Message(m) => m.clone(),
            FauxResponseStep::Factory(f) => {
                let state = self.state();
                f(context, options, &state, model).await?
            }
        };
        let mut cloned = resolved;
        cloned.api = self.api.clone();
        cloned.provider = self.provider.clone();
        cloned.model = model.id.clone();
        Ok(with_usage_estimate(cloned, context, options, &self.prompt_cache))
    }

    fn run_stream(self: &Arc<Self>, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        let (sender, outer) = AssistantMessageEventStream::new();
        let step = self.pending.lock().pop_front();
        self.state.lock().call_count += 1;
        let this = self.clone();
        let model = model.clone();
        let context = context.clone();
        crate::utils::wasm::spawn(async move {
            let result: anyhow::Result<()> = async {
                if let Some(on_response) = &options.on_response {
                    on_response(ProviderResponse { status: 200, headers: Default::default() }, model.clone()).await;
                }
                let Some(step) = step else {
                    let message = create_error_message("No more faux responses queued", &this.api, &this.provider, &model.id);
                    let message = with_usage_estimate(message, &context, &options, &this.prompt_cache);
                    sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: message.clone() });
                    sender.end(Some(message));
                    return Ok(());
                };
                if options.deferred.as_ref().is_some_and(|d| !matches!(d, crate::types::DeferredOption::Enabled(false))) {
                    let handle = DeferredHandle {
                        provider: model.provider.clone(),
                        model_id: model.id.clone(),
                        api: model.api.clone(),
                        id: random_id("deferred"),
                        expires_at: None,
                        poll_after_ms: this.deferred_options.and_then(|d| d.poll_after_ms),
                        data: None,
                    };
                    this.deferred.lock().insert(
                        handle.id.clone(),
                        DeferredEntry {
                            handle: handle.clone(),
                            step,
                            context: context.clone(),
                            options: options.clone(),
                            model: model.clone(),
                            pending_fetches: this.deferred_options.and_then(|d| d.pending_fetches).unwrap_or(0),
                            cancelled: false,
                            final_message: None,
                        },
                    );
                    return stream_with_deltas(
                        &sender,
                        create_deferred_message(&model, handle),
                        this.min_token_size,
                        this.max_token_size,
                        this.tokens_per_second,
                        options.signal.clone(),
                    )
                    .await;
                }
                let message = this.resolve_response(&step, &context, &options, &model).await?;
                stream_with_deltas(
                    &sender,
                    message,
                    this.min_token_size,
                    this.max_token_size,
                    this.tokens_per_second,
                    options.signal.clone(),
                )
                .await
            }
            .await;
            if let Err(error) = result {
                let message = create_error_message(&error.to_string(), &this.api, &this.provider, &model.id);
                sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: message.clone() });
                sender.end(Some(message));
            }
        });
        outer
    }

    fn run_fetch_deferred(self: &Arc<Self>, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        let (sender, outer) = AssistantMessageEventStream::new();
        self.state.lock().deferred_fetch_count += 1;
        let this = self.clone();
        let model = model.clone();
        let handle = handle.clone();
        crate::utils::wasm::spawn(async move {
            let result: anyhow::Result<()> = async {
                if let Some(on_response) = &options.on_response {
                    on_response(ProviderResponse { status: 200, headers: Default::default() }, model.clone()).await;
                }
                #[allow(clippy::large_enum_variant)]
                enum Next {
                    Pending(DeferredHandle),
                    Final(AssistantMessage),
                    Resolve(FauxResponseStep, TranscriptContext, StreamOptions, Model),
                }
                let next = {
                    let mut deferred = this.deferred.lock();
                    let entry = deferred.get_mut(&handle.id).filter(|e| {
                        e.handle.provider == handle.provider && e.handle.model_id == handle.model_id && e.handle.api == handle.api
                    });
                    let Some(entry) = entry else {
                        anyhow::bail!("Unknown faux deferred response: {}", handle.id);
                    };
                    if entry.cancelled {
                        anyhow::bail!("Faux deferred response was cancelled: {}", handle.id);
                    }
                    if entry.pending_fetches > 0 {
                        entry.pending_fetches -= 1;
                        Next::Pending(entry.handle.clone())
                    } else if let Some(final_message) = &entry.final_message {
                        Next::Final(final_message.clone())
                    } else {
                        let mut submission = entry.options.clone();
                        submission.deferred = None;
                        submission.signal = None;
                        submission.on_response = None;
                        Next::Resolve(entry.step.clone(), entry.context.clone(), submission, entry.model.clone())
                    }
                };
                let message = match next {
                    Next::Pending(h) => create_deferred_message(&model, h),
                    Next::Final(m) => m,
                    Next::Resolve(step, context, submission, entry_model) => {
                        let final_message = match this.resolve_response(&step, &context, &submission, &entry_model).await {
                            Ok(m) => m,
                            Err(e) => create_error_message(&e.to_string(), &this.api, &this.provider, &entry_model.id),
                        };
                        if let Some(entry) = this.deferred.lock().get_mut(&handle.id) {
                            entry.final_message = Some(final_message.clone());
                        }
                        final_message
                    }
                };
                stream_with_deltas(
                    &sender,
                    message,
                    this.min_token_size,
                    this.max_token_size,
                    this.tokens_per_second,
                    options.signal.clone(),
                )
                .await
            }
            .await;
            if let Err(error) = result {
                let message = create_error_message(&error.to_string(), &this.api, &this.provider, &model.id);
                sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: message.clone() });
                sender.end(Some(message));
            }
        });
        outer
    }
}

/// [`ProviderStreams`] adapter over an `Arc<FauxCore>`.
pub struct FauxStreams(pub Arc<FauxCore>);

impl ProviderStreams for FauxStreams {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.run_stream(model, context, options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.run_stream(model, context, options)
    }

    fn supports_fetch_deferred(&self) -> bool {
        true
    }

    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        self.0.run_fetch_deferred(model, handle, options)
    }

    fn supports_cancel_deferred(&self) -> bool {
        true
    }

    fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> BoxFuture<'static, anyhow::Result<()>> {
        let core = self.0.clone();
        let model = model.clone();
        let handle = handle.clone();
        Box::pin(async move {
            core.state.lock().cancelled_deferred.push(handle.clone());
            if let Some(entry) = core.deferred.lock().get_mut(&handle.id) {
                entry.cancelled = true;
            }
            if let Some(on_response) = &options.on_response {
                on_response(ProviderResponse { status: 200, headers: Default::default() }, model).await;
            }
            Ok(())
        })
    }
}

struct FauxApiKeyAuth;

#[async_trait]
impl ApiKeyAuth for FauxApiKeyAuth {
    fn name(&self) -> &str {
        "Faux"
    }
    async fn resolve(&self, _input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        Ok(Some(AuthResult::default()))
    }
}

/// Handle of a faux provider built for an explicit `Models` collection.
#[derive(Clone)]
pub struct FauxProviderHandle {
    pub provider: Arc<dyn Provider>,
    pub core: Arc<FauxCore>,
}

impl FauxProviderHandle {
    pub fn api(&self) -> &str {
        &self.core.api
    }
    pub fn models(&self) -> &[Model] {
        &self.core.models
    }
    /// The first model.
    pub fn get_model(&self) -> Model {
        self.core.get_model(None).expect("faux provider has at least one model")
    }
    pub fn get_model_by_id(&self, id: &str) -> Option<Model> {
        self.core.get_model(Some(id))
    }
    pub fn state(&self) -> FauxProviderState {
        self.core.state()
    }
    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.set_responses(responses)
    }
    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.append_responses(responses)
    }
    pub fn get_pending_response_count(&self) -> usize {
        self.core.get_pending_response_count()
    }
}

/// Faux provider for tests built on explicit `Models` collections.
pub fn faux_provider(options: RegisterFauxProviderOptions) -> FauxProviderHandle {
    let core = FauxCore::new(&options);
    let provider = create_provider(CreateProviderOptions {
        id: core.provider.clone(),
        auth: ProviderAuth { api_key: Some(Arc::new(FauxApiKeyAuth)), oauth: None },
        models: core.models.iter().cloned().map(AnyModel::Chat).collect(),
        api: Some(ProviderApi::Single(Arc::new(FauxStreams(core.clone())))),
        ..Default::default()
    })
    .expect("faux provider has an api");
    FauxProviderHandle { provider, core }
}
