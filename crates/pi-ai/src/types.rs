//! Core types (port of `types.ts`).
//!
//! JSON shapes match the TypeScript implementation exactly: camelCase field names,
//! `type`/`role` discriminants and omitted optional fields.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

pub use crate::providers::compat_schema::{
    AnthropicAllowedFallbackModel, AnthropicMessagesCompat, BedrockCompat, ChatTemplateKwargValue,
    MistralConversationsCompat, ModelCompat, OpenAICompletionsCompat, OpenAIResponsesCompat,
    OpenRouterRouting, ProviderCompat, SessionAffinityFormat, ThinkingTokenBudgetField,
    VercelGatewayRouting,
};
pub use crate::providers::model_schema::{
    CacheRetention, ModelCost, ModelCostRates, ModelCostTier, ModelImageInputLimits,
    ModelImageResizeOptions, ModelInputLimits, ModelInputModality, ModelPromptCache,
    ModelThinkingLevel, ThinkingLevel, ThinkingLevelMap,
};
pub use crate::utils::diagnostics::AssistantMessageDiagnostic;
pub use crate::utils::event_stream::AssistantMessageEventStream;
pub use crate::utils::identity::ClientIdentity;
use crate::serde_util::{
    deserialize_some, js_f64, lax_i64, lax_u64, null_as_default, opt_lax_u64,
};

// ---------------------------------------------------------------------------
// API / provider identifiers
// ---------------------------------------------------------------------------

/// API identifier. Known values are listed in [`known_api`]; custom strings are allowed.
pub type Api = String;
/// Image generation API identifier (see [`known_api::OPENROUTER_IMAGES`]).
pub type ImageApi = String;
/// Classifier API identifier.
pub type ClassifierApi = String;
/// Provider identifier. Known values are listed in [`known_provider`].
pub type ProviderId = String;

/// Known API identifiers (`KnownApi`, `KnownImageApi`, `KnownClassifierApi`).
pub mod known_api {
    pub const OPENAI_COMPLETIONS: &str = "openai-completions";
    pub const MISTRAL_CONVERSATIONS: &str = "mistral-conversations";
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    pub const AZURE_OPENAI_RESPONSES: &str = "azure-openai-responses";
    pub const OPENAI_CODEX_RESPONSES: &str = "openai-codex-responses";
    pub const ANTHROPIC_MESSAGES: &str = "anthropic-messages";
    pub const BEDROCK_CONVERSE_STREAM: &str = "bedrock-converse-stream";
    pub const GOOGLE_GENERATIVE_AI: &str = "google-generative-ai";
    pub const GOOGLE_VERTEX: &str = "google-vertex";
    pub const PI_MESSAGES: &str = "pi-messages";

    pub const KNOWN_APIS: [&str; 10] = [
        OPENAI_COMPLETIONS,
        MISTRAL_CONVERSATIONS,
        OPENAI_RESPONSES,
        AZURE_OPENAI_RESPONSES,
        OPENAI_CODEX_RESPONSES,
        ANTHROPIC_MESSAGES,
        BEDROCK_CONVERSE_STREAM,
        GOOGLE_GENERATIVE_AI,
        GOOGLE_VERTEX,
        PI_MESSAGES,
    ];

    pub const OPENROUTER_IMAGES: &str = "openrouter-images";

    pub const TYPESAFE_SYSTEM_ONE: &str = "typesafe-system-one";
    pub const CLOUDFLARE_WORKERS_AI_SYSTEM_ONE: &str = "cloudflare-workers-ai-system-one";
    pub const LLAMA_CPP_CLASSIFY: &str = "llama-cpp-classify";
    pub const OPENAI_DECISIONS: &str = "openai-decisions";
}

/// Known provider identifiers (`KnownProvider`).
pub mod known_provider {
    pub const KNOWN_PROVIDERS: [&str; 42] = [
        "amazon-bedrock",
        "ant-ling",
        "anthropic",
        "google",
        "google-vertex",
        "openai",
        "azure",
        "openai-codex",
        "radius",
        "typesafe",
        "nvidia",
        "deepseek",
        "github-copilot",
        "xai",
        "groq",
        "cerebras",
        "openrouter",
        "vercel-ai-gateway",
        "zai",
        "zai-coding-cn",
        "mistral",
        "minimax",
        "minimax-cn",
        "moonshotai",
        "moonshotai-cn",
        "huggingface",
        "fireworks",
        "together",
        "baseten",
        "opencode",
        "opencode-go",
        "kimi-coding",
        "meta",
        "cloudflare-workers-ai",
        "cloudflare-ai-gateway",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ];
}

// ---------------------------------------------------------------------------
// JSON aliases
// ---------------------------------------------------------------------------

pub type JsonValue = Value;
pub type JsonObject = Map<String, Value>;
pub type SamplingParams = Map<String, Value>;
pub type SamplingParamsByThinkingLevel = BTreeMap<ModelThinkingLevel, SamplingParams>;

// ---------------------------------------------------------------------------
// Request options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
}

/// Token budgets for each thinking level (token-based providers only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

/// Provider-scoped environment overrides. Values take precedence over the process env.
pub type ProviderEnv = HashMap<String, String>;
/// Request headers; a `None` value suppresses a default header with the same name.
pub type ProviderHeaders = IndexMap<String, Option<String>>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
}

/// Callback for inspecting or replacing provider payloads before sending.
/// Return `None` to keep the payload unchanged.
pub type OnPayload =
    Arc<dyn Fn(Value, Model) -> BoxFuture<'static, Option<Value>> + Send + Sync>;
/// Callback invoked after an HTTP response is received and before its body is consumed.
pub type OnResponse = Arc<dyn Fn(ProviderResponse, Model) -> BoxFuture<'static, ()> + Send + Sync>;
/// Observer for each parsed provider stream event before normalization. Providers await the
/// returned future before processing the event, so slow observers delay stream consumption
/// (TypeScript `onProviderStreamEvent` returning a promise).
pub type OnProviderStreamEvent =
    Arc<dyn Fn(Value, Model) -> BoxFuture<'static, ()> + Send + Sync>;

/// Deferred-response request window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeferredOption {
    Enabled(bool),
    Window {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
}

/// Options shared by every stream entry point.
///
/// TypeScript splits these into `ProviderRequestOptions`, `StreamOptions` and
/// `SimpleStreamOptions`; Rust uses one struct. The "simple" fields (`reasoning`,
/// `tool_choice`, `thinking_budgets`, `deferred`) are read by `stream_simple` only.
/// API-specific options (e.g. Anthropic's `thinkingEnabled`) go into `extra` under their
/// TypeScript camelCase names.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub signal: Option<CancellationToken>,
    /// Explicit parent context for telemetry produced by this logical request.
    pub telemetry: Option<pi_telemetry::ContextRef>,
    pub api_key: Option<String>,
    /// Custom HTTP client (the `fetch` option equivalent).
    pub http_client: Option<reqwest::Client>,
    pub env: Option<ProviderEnv>,
    pub on_payload: Option<OnPayload>,
    pub on_response: Option<OnResponse>,
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub headers: Option<ProviderHeaders>,
    pub identity: Option<ClientIdentity>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub sampling_params: Option<SamplingParams>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: Option<Map<String, Value>>,
    // ---- SimpleStreamOptions ----
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub deferred: Option<DeferredOption>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// API-specific options keyed by their TypeScript names.
    pub extra: Map<String, Value>,
}

pub type SimpleStreamOptions = StreamOptions;
pub type ProviderStreamOptions = StreamOptions;
pub type ProviderRequestOptions = StreamOptions;
pub type DeferredFetchOptions = StreamOptions;
pub type DeferredCancelOptions = StreamOptions;

impl StreamOptions {
    pub fn is_aborted(&self) -> bool {
        self.signal.as_ref().is_some_and(|s| s.is_cancelled())
    }

    /// Read an API-specific option.
    pub fn extra_value(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }

    pub fn extra_bool(&self, key: &str) -> Option<bool> {
        self.extra.get(key).and_then(Value::as_bool)
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }

    pub fn extra_u64(&self, key: &str) -> Option<u64> {
        self.extra.get(key).and_then(Value::as_u64)
    }

    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    /// The deferred long-poll duration (`wait`) for deferred fetches.
    pub fn wait_ms(&self) -> Option<u64> {
        self.extra_u64("wait")
    }
}

impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("signal", &self.signal.as_ref().map(|s| s.is_cancelled()))
            .field("telemetry", &self.telemetry.is_some())
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("env", &self.env.as_ref().map(|e| e.keys().collect::<Vec<_>>()))
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
            .field("on_provider_stream_event", &self.on_provider_stream_event.is_some())
            .field("headers", &self.headers.as_ref().map(|h| h.keys().collect::<Vec<_>>()))
            .field("identity", &self.identity)
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("temperature", &self.temperature)
            .field("sampling_params", &self.sampling_params)
            .field("max_tokens", &self.max_tokens)
            .field("transport", &self.transport)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("metadata", &self.metadata)
            .field("tool_choice", &self.tool_choice)
            .field("reasoning", &self.reasoning)
            .field("deferred", &self.deferred)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("extra", &self.extra)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextPhase {
    Commentary,
    FinalAnswer,
}

/// Structured text signature (`TextSignatureV1`), serialized into `TextContent.textSignature`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSignatureV1 {
    pub v: u8,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TextPhase>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    /// Provider message metadata (legacy id string or `TextSignatureV1` JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
    /// Set by a caller whose JavaScript text lost unpaired UTF-16 surrogates on the way to
    /// this UTF-8 string. Upstream checks length/`trim()` before `sanitizeSurrogates`, so such
    /// text still counts as non-blank there. Never serialized; always false for native text.
    #[serde(skip)]
    pub lone_surrogates_removed: bool,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
    /// Upstream's `text.trim().length === 0` before sanitizing.
    pub fn is_blank(&self) -> bool {
        is_blank_text(&self.text, self.lone_surrogates_removed)
    }
    /// Upstream's `text.length === 0` before sanitizing.
    pub fn is_empty_text(&self) -> bool {
        self.text.is_empty() && !self.lone_surrogates_removed
    }
}

/// `text.trim().length === 0` for JavaScript text that may have lost unpaired
/// surrogates, which are not whitespace, before reaching this string.
pub fn is_blank_text(text: &str, lone_surrogates_removed: bool) -> bool {
    text.trim().is_empty() && !lone_surrogates_removed
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    /// Provider-specific opaque or serialized reasoning replay data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// When true, the thinking content was redacted by safety filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
    /// Set by a caller whose JavaScript text lost unpaired UTF-16 surrogates on the way to
    /// this UTF-8 string. Upstream checks length/`trim()` before `sanitizeSurrogates`, so such
    /// text still counts as non-blank there. Never serialized; always false for native text.
    #[serde(skip)]
    pub lone_surrogates_removed: bool,
}

impl ThinkingContent {
    pub fn new(thinking: impl Into<String>) -> Self {
        Self { thinking: thinking.into(), ..Default::default() }
    }
    /// Upstream's `thinking.trim().length === 0` before sanitizing.
    pub fn is_blank(&self) -> bool {
        is_blank_text(&self.thinking, self.lone_surrogates_removed)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64 encoded image data.
    pub data: String,
    /// e.g. "image/jpeg", "image/png".
    pub mime_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub arguments: JsonObject,
    /// Google-specific: opaque signature for reusing thought context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// OpenAI Responses namespace for calls to dynamically loaded or namespaced tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Set when the model's complete arguments text was not a valid JSON object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

/// Content of an assistant message: text, thinking or tool calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantContent {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "thinking")]
    Thinking(ThinkingContent),
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}

impl AssistantContent {
    pub fn as_text(&self) -> Option<&TextContent> {
        match self {
            AssistantContent::Text(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_thinking(&self) -> Option<&ThinkingContent> {
        match self {
            AssistantContent::Thinking(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            AssistantContent::ToolCall(t) => Some(t),
            _ => None,
        }
    }
    pub fn type_name(&self) -> &'static str {
        match self {
            AssistantContent::Text(_) => "text",
            AssistantContent::Thinking(_) => "thinking",
            AssistantContent::ToolCall(_) => "toolCall",
        }
    }
}

/// Text or image content (user messages, tool results, image I/O).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UserContentBlock {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "image")]
    Image(ImageContent),
}

impl UserContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        UserContentBlock::Text(TextContent::new(text))
    }
    pub fn as_text(&self) -> Option<&TextContent> {
        match self {
            UserContentBlock::Text(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_image(&self) -> Option<&ImageContent> {
        match self {
            UserContentBlock::Image(i) => Some(i),
            _ => None,
        }
    }
}

pub type ToolResultContent = UserContentBlock;
pub type ImagesInputContent = UserContentBlock;
pub type ImagesOutputContent = UserContentBlock;

/// `string | (TextContent | ImageContent)[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

impl Default for UserContent {
    fn default() -> Self {
        UserContent::Blocks(Vec::new())
    }
}

impl From<String> for UserContent {
    fn from(value: String) -> Self {
        UserContent::Text(value)
    }
}

impl From<&str> for UserContent {
    fn from(value: &str) -> Self {
        UserContent::Text(value.to_string())
    }
}

impl From<Vec<UserContentBlock>> for UserContent {
    fn from(value: Vec<UserContentBlock>) -> Self {
        UserContent::Blocks(value)
    }
}

/// `string | TextContent[]` for system messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<SystemTextBlock>),
}

impl Default for SystemContent {
    fn default() -> Self {
        SystemContent::Text(String::new())
    }
}

impl From<String> for SystemContent {
    fn from(value: String) -> Self {
        SystemContent::Text(value)
    }
}

impl From<&str> for SystemContent {
    fn from(value: &str) -> Self {
        SystemContent::Text(value.to_string())
    }
}

/// A `TextContent` block with its `type: "text"` discriminant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SystemTextBlock {
    #[serde(rename = "text")]
    Text(TextContent),
}

impl SystemTextBlock {
    pub fn text(&self) -> &str {
        match self {
            SystemTextBlock::Text(t) => &t.text,
        }
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    #[serde(with = "js_f64")]
    pub input: f64,
    #[serde(with = "js_f64")]
    pub output: f64,
    #[serde(with = "js_f64")]
    pub cache_read: f64,
    #[serde(with = "js_f64")]
    pub cache_write: f64,
    #[serde(with = "js_f64")]
    pub total: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(with = "lax_u64")]
    pub input: u64,
    #[serde(with = "lax_u64")]
    pub output: u64,
    #[serde(with = "lax_u64")]
    pub cache_read: u64,
    #[serde(with = "lax_u64")]
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention. Only Anthropic reports this split.
    #[serde(rename = "cacheWrite1h", default, skip_serializing_if = "Option::is_none", with = "opt_lax_u64")]
    pub cache_write_1h: Option<u64>,
    /// Reasoning tokens, a subset of `output`, when the provider reports them.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_lax_u64")]
    pub reasoning: Option<u64>,
    #[serde(with = "lax_u64")]
    pub total_tokens: u64,
    pub cost: UsageCost,
}

impl Usage {
    /// All-zero usage.
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Pending => "pending",
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "toolUse",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
            StopReason::Deferred => "deferred",
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    /// Provider token, such as a response id or batch id plus row id.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    /// Provider conversion data required to reconstruct the final assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_some")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// System instructions and tool declarations at one point in the transcript.
///
/// JSON key order follows how TS builds the message: `role`, `content`, `sections`, then
/// `timestamp` before `toolsAdded`/`toolsRemoved` (the agent loop's tool-change messages), or after
/// them when [`timestamp_last`](Self::timestamp_last) is set (`createInitialSystemMessage`,
/// `getCurrentSystemMessage`). Parsing records which order the input used, so a message is
/// written back as read.
#[derive(Debug, Clone, Default)]
pub struct SystemMessage {
    pub content: SystemContent,
    /// Named, ordered prompt sections; `None` (null) removes one.
    pub sections: Option<IndexMap<String, Option<String>>>,
    pub tools_added: Option<Vec<Tool>>,
    pub tools_removed: Option<Vec<ToolReference>>,
    pub timestamp: i64,
    /// Serialize `timestamp` after the tool fields. Only affects key order; ignored by `==`.
    pub timestamp_last: bool,
    /// For string `content`, as [`TextContent::lone_surrogates_removed`]. Never serialized.
    pub content_lone_surrogates_removed: bool,
    /// Names of sections whose value lost unpaired surrogates, as
    /// [`TextContent::lone_surrogates_removed`]. Never serialized.
    pub sections_lone_surrogates_removed: Vec<String>,
}

impl SystemMessage {
    /// Upstream's `contentText(content).length === 0` before sanitizing.
    pub fn content_is_empty(&self) -> bool {
        match &self.content {
            SystemContent::Text(text) => text.is_empty() && !self.content_lone_surrogates_removed,
            SystemContent::Blocks(blocks) => match blocks.as_slice() {
                [] => true,
                [SystemTextBlock::Text(block)] => block.is_empty_text(),
                // Joined with "\n", so never empty.
                _ => false,
            },
        }
    }
    /// Whether the content lost unpaired surrogates.
    pub fn content_lone_surrogates_removed(&self) -> bool {
        match &self.content {
            SystemContent::Text(_) => self.content_lone_surrogates_removed,
            SystemContent::Blocks(blocks) => {
                blocks.iter().any(|SystemTextBlock::Text(block)| block.lone_surrogates_removed)
            }
        }
    }
    /// Whether section `name`'s value lost unpaired surrogates.
    pub fn section_lone_surrogates_removed(&self, name: &str) -> bool {
        self.sections_lone_surrogates_removed.iter().any(|removed| removed == name)
    }
}

impl PartialEq for SystemMessage {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && self.sections == other.sections
            && self.tools_added == other.tools_added
            && self.tools_removed == other.tools_removed
            && self.timestamp == other.timestamp
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SystemMessageWire {
    #[serde(default, deserialize_with = "null_as_default")]
    content: SystemContent,
    #[serde(default)]
    sections: Option<IndexMap<String, Option<String>>>,
    #[serde(default)]
    tools_added: Option<Vec<Tool>>,
    #[serde(default)]
    tools_removed: Option<Vec<ToolReference>>,
    #[serde(with = "lax_i64")]
    timestamp: i64,
}

impl Serialize for SystemMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        struct Timestamp(i64);
        impl Serialize for Timestamp {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                lax_i64::serialize(&self.0, serializer)
            }
        }
        let len = 2
            + usize::from(self.sections.is_some())
            + usize::from(self.tools_added.is_some())
            + usize::from(self.tools_removed.is_some());
        let mut state = serializer.serialize_struct("SystemMessage", len)?;
        state.serialize_field("content", &self.content)?;
        if let Some(sections) = &self.sections {
            state.serialize_field("sections", sections)?;
        }
        if !self.timestamp_last {
            state.serialize_field("timestamp", &Timestamp(self.timestamp))?;
        }
        if let Some(tools) = &self.tools_added {
            state.serialize_field("toolsAdded", tools)?;
        }
        if let Some(tools) = &self.tools_removed {
            state.serialize_field("toolsRemoved", tools)?;
        }
        if self.timestamp_last {
            state.serialize_field("timestamp", &Timestamp(self.timestamp))?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for SystemMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(deserializer)?;
        let position = |key: &str| map.keys().position(|k| k == key);
        let timestamp_last = match (position("timestamp"), position("toolsAdded"), position("toolsRemoved")) {
            (Some(timestamp), added, removed) => added.into_iter().chain(removed).any(|tools| tools < timestamp),
            _ => false,
        };
        let wire = SystemMessageWire::deserialize(Value::Object(map)).map_err(D::Error::custom)?;
        Ok(SystemMessage {
            content: wire.content,
            sections: wire.sections,
            tools_added: wire.tools_added,
            tools_removed: wire.tools_removed,
            timestamp: wire.timestamp,
            timestamp_last,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: UserContent,
    #[serde(with = "lax_i64")]
    pub timestamp: i64,
    /// For string `content`, as [`TextContent::lone_surrogates_removed`].
    #[serde(skip)]
    pub lone_surrogates_removed: bool,
}

impl UserMessage {
    pub fn new(content: impl Into<UserContent>, timestamp: i64) -> Self {
        Self { content: content.into(), timestamp, lone_surrogates_removed: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    /// Concrete model reported by the provider when different from the requested `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider-specific response/message identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Exact provider-native effort level used for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<String>,
    /// Pi thinking level the agent loop requested for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ModelThinkingLevel>,
    /// Redacted provider/runtime diagnostics for failures and recoveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    /// Provider indication of whether the model explicitly ended its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// Unix timestamp in milliseconds when the request started.
    #[serde(with = "lax_i64")]
    pub timestamp: i64,
    /// Milliseconds from `timestamp` until the response ended (monotonic clock).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_lax_u64")]
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    /// An empty assistant message for `model` with zero usage.
    pub fn empty_for(model: &Model, timestamp: i64) -> Self {
        Self {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            timestamp,
            ..Default::default()
        }
    }

    /// Concatenated text of all text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(AssistantContent::as_text)
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(AssistantContent::as_tool_call)
    }
}

/// A tool call that another tool made while it ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCallRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<u64>,
    pub status: NestedToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NestedToolCallStatus {
    Ok,
    Error,
    Unfinished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCallRecord>,
    pub complete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: Vec<UserContentBlock>,
    /// Tool-specific JSON details. An explicit `null` is preserved.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_some")]
    pub details: Option<Value>,
    /// Usage from the tool execution itself, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Calls this tool made to other tools. Not sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedToolCalls>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(with = "lax_i64")]
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_lax_u64")]
    pub duration_ms: Option<u64>,
}

/// A transcript message, discriminated by `role`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::System(_) => "system",
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            Message::System(m) => m.timestamp,
            Message::User(m) => m.timestamp,
            Message::Assistant(m) => m.timestamp,
            Message::ToolResult(m) => m.timestamp,
        }
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_user(&self) -> Option<&UserMessage> {
        match self {
            Message::User(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            Message::System(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_tool_result(&self) -> Option<&ToolResultMessage> {
        match self {
            Message::ToolResult(m) => Some(m),
            _ => None,
        }
    }
}

impl From<SystemMessage> for Message {
    fn from(value: SystemMessage) -> Self {
        Message::System(value)
    }
}
impl From<UserMessage> for Message {
    fn from(value: UserMessage) -> Self {
        Message::User(value)
    }
}
impl From<AssistantMessage> for Message {
    fn from(value: AssistantMessage) -> Self {
        Message::Assistant(value)
    }
}
impl From<ToolResultMessage> for Message {
    fn from(value: ToolResultMessage) -> Self {
        Message::ToolResult(value)
    }
}

// ---------------------------------------------------------------------------
// Image generation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<UserContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesStopReason {
    #[default]
    Stop,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: ImageApi,
    pub provider: ProviderId,
    pub model: String,
    pub output: Vec<UserContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(with = "lax_i64")]
    pub timestamp: i64,
}

pub type ImagesOptions = StreamOptions;
pub type ProviderImagesOptions = StreamOptions;
pub type ClassifierOptions = StreamOptions;

// ---------------------------------------------------------------------------
// Classifiers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierBoolCriteria {
    #[serde(rename = "true")]
    pub true_: String,
    #[serde(rename = "false")]
    pub false_: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierQuestion {
    Choice { instructions: String, criteria: IndexMap<String, String> },
    Score { instructions: String, criteria: Vec<String> },
    Bool { instructions: String, criteria: ClassifierBoolCriteria },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierContext {
    pub state: JsonObject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    pub questions: IndexMap<String, ClassifierQuestion>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierAnswer {
    Choice {
        choice: String,
        probabilities: IndexMap<String, f64>,
        #[serde(with = "js_f64")]
        confidence: f64,
    },
    Score {
        #[serde(with = "js_f64")]
        score: f64,
        #[serde(with = "js_f64")]
        confidence: f64,
    },
    Bool {
        #[serde(with = "js_f64")]
        probability: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierStopReason {
    #[default]
    Stop,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    pub api: ClassifierApi,
    pub provider: ProviderId,
    pub model: String,
    pub answers: IndexMap<String, ClassifierAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ClassifierStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(with = "lax_i64")]
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarFormat {
    OpenaiLark,
    OpenaiRegex,
}

pub type GrammarVariants = BTreeMap<GrammarFormat, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonSchemaStrictness {
    Prefer,
    Require,
}

/// Provider-side constrained sampling configuration for a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    JsonSchema { strict: JsonSchemaStrictness },
    Grammar { variants: GrammarVariants },
}

/// `false | ConstrainedSamplingConfig`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstrainedSampling {
    Disabled,
    Config(ConstrainedSamplingConfig),
}

impl ConstrainedSampling {
    pub fn config(&self) -> Option<&ConstrainedSamplingConfig> {
        match self {
            ConstrainedSampling::Disabled => None,
            ConstrainedSampling::Config(c) => Some(c),
        }
    }
}

impl Serialize for ConstrainedSampling {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ConstrainedSampling::Disabled => serializer.serialize_bool(false),
            ConstrainedSampling::Config(c) => c.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ConstrainedSampling {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Bool(false) => Ok(ConstrainedSampling::Disabled),
            Value::Bool(true) => Err(D::Error::custom("constrainedSampling must be false or a config")),
            other => serde_json::from_value(other)
                .map(ConstrainedSampling::Config)
                .map_err(D::Error::custom),
        }
    }
}

/// A tool definition. `parameters` is a JSON Schema (TypeBox schemas are JSON Schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSampling>,
    /// The parameters were built with TypeBox 1.x (TS: the schema carries the non-enumerable
    /// `~kind`), so argument validation applies `Value.Convert` before the JSON Schema coercion.
    #[serde(skip)]
    pub typebox: bool,
}

impl Tool {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self { name: name.into(), description: description.into(), parameters, constrained_sampling: None, typebox: false }
    }

    /// Mark the parameters as built with TypeBox (see [`Tool::typebox`]).
    pub fn with_typebox(mut self, typebox: bool) -> Self {
        self.typebox = typebox;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: String,
}

/// Request input accepted by the public stream entry points.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

/// Normalized request context passed to providers. Only
/// [`normalize_context`](crate::utils::transcript::normalize_context) produces it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[non_exhaustive]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
}

impl TranscriptContext {
    pub(crate) fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Event protocol for [`AssistantMessageEventStream`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    /// `reason` is one of stop, length, toolUse, deferred.
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    /// `reason` is aborted or error.
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        }
    }

    /// The live partial message (or final message for done/error).
    pub fn message(&self) -> &AssistantMessage {
        match self {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
            AssistantMessageEvent::Done { message, .. } => message,
            AssistantMessageEvent::Error { error, .. } => error,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. })
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// Chat model: usable with `stream()` and friends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    pub input: Vec<ModelInputModality>,
    pub cost: ModelCost,
    #[serde(default, with = "lax_u64")]
    pub context_window: u64,
    #[serde(default, with = "lax_u64")]
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Compatibility overrides. Which fields apply depends on `api`. Boxed: the struct is large
    /// and absent from most catalog models, of which several copies are held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Box<ModelCompat>>,
    /// Maps pi thinking levels to provider/model-specific values; `None` marks unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<SamplingParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    /// Always `"chat"` when present.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub model_type: Option<String>,
    /// Unmodeled catalog fields, preserved for round-tripping.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.contains(&ModelInputModality::Image)
    }

    pub fn compat(&self) -> ModelCompat {
        self.compat.as_deref().cloned().unwrap_or_default()
    }
}

/// Image-generation model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageModel {
    pub id: String,
    pub name: String,
    pub api: ImageApi,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<ModelInputModality>,
    pub output: Vec<String>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Always `"image"`.
    #[serde(rename = "type")]
    pub model_type: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Structured classifier model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierModel {
    pub id: String,
    pub name: String,
    pub api: ClassifierApi,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<ModelInputModality>,
    pub cost: ModelCost,
    #[serde(default, with = "lax_u64")]
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Always `"classifier"`.
    #[serde(rename = "type")]
    pub model_type: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    Chat,
    Image,
    Classifier,
}

impl ModelType {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelType::Chat => "chat",
            ModelType::Image => "image",
            ModelType::Classifier => "classifier",
        }
    }
}

/// Anything a provider can list.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AnyModel {
    Chat(Model),
    Image(ImageModel),
    Classifier(ClassifierModel),
}

impl AnyModel {
    pub fn model_type(&self) -> ModelType {
        match self {
            AnyModel::Chat(_) => ModelType::Chat,
            AnyModel::Image(_) => ModelType::Image,
            AnyModel::Classifier(_) => ModelType::Classifier,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            AnyModel::Chat(m) => &m.id,
            AnyModel::Image(m) => &m.id,
            AnyModel::Classifier(m) => &m.id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            AnyModel::Chat(m) => &m.name,
            AnyModel::Image(m) => &m.name,
            AnyModel::Classifier(m) => &m.name,
        }
    }
    pub fn api(&self) -> &str {
        match self {
            AnyModel::Chat(m) => &m.api,
            AnyModel::Image(m) => &m.api,
            AnyModel::Classifier(m) => &m.api,
        }
    }
    pub fn provider(&self) -> &str {
        match self {
            AnyModel::Chat(m) => &m.provider,
            AnyModel::Image(m) => &m.provider,
            AnyModel::Classifier(m) => &m.provider,
        }
    }
    pub fn base_url(&self) -> &str {
        match self {
            AnyModel::Chat(m) => &m.base_url,
            AnyModel::Image(m) => &m.base_url,
            AnyModel::Classifier(m) => &m.base_url,
        }
    }
    pub fn set_base_url(&mut self, base_url: String) {
        match self {
            AnyModel::Chat(m) => m.base_url = base_url,
            AnyModel::Image(m) => m.base_url = base_url,
            AnyModel::Classifier(m) => m.base_url = base_url,
        }
    }
    pub fn cost(&self) -> &ModelCost {
        match self {
            AnyModel::Chat(m) => &m.cost,
            AnyModel::Image(m) => &m.cost,
            AnyModel::Classifier(m) => &m.cost,
        }
    }
    pub fn input(&self) -> &[ModelInputModality] {
        match self {
            AnyModel::Chat(m) => &m.input,
            AnyModel::Image(m) => &m.input,
            AnyModel::Classifier(m) => &m.input,
        }
    }
    pub fn headers(&self) -> Option<&IndexMap<String, String>> {
        match self {
            AnyModel::Chat(m) => m.headers.as_ref(),
            AnyModel::Image(m) => m.headers.as_ref(),
            AnyModel::Classifier(m) => m.headers.as_ref(),
        }
    }
    pub fn as_chat(&self) -> Option<&Model> {
        match self {
            AnyModel::Chat(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_image(&self) -> Option<&ImageModel> {
        match self {
            AnyModel::Image(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_classifier(&self) -> Option<&ClassifierModel> {
        match self {
            AnyModel::Classifier(m) => Some(m),
            _ => None,
        }
    }
    pub fn into_chat(self) -> Option<Model> {
        match self {
            AnyModel::Chat(m) => Some(m),
            _ => None,
        }
    }
}

impl From<Model> for AnyModel {
    fn from(value: Model) -> Self {
        AnyModel::Chat(value)
    }
}
impl From<ImageModel> for AnyModel {
    fn from(value: ImageModel) -> Self {
        AnyModel::Image(value)
    }
}
impl From<ClassifierModel> for AnyModel {
    fn from(value: ClassifierModel) -> Self {
        AnyModel::Classifier(value)
    }
}

impl Serialize for AnyModel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AnyModel::Chat(m) => m.serialize(serializer),
            AnyModel::Image(m) => m.serialize(serializer),
            AnyModel::Classifier(m) => m.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AnyModel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        AnyModel::from_value(value).map_err(D::Error::custom)
    }
}

impl AnyModel {
    /// The `type` of a raw model value: missing means `"chat"`.
    pub fn raw_type(value: &Value) -> Option<&str> {
        match value.get("type") {
            None | Some(Value::Null) => Some("chat"),
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => None,
        }
    }

    pub fn from_value(value: Value) -> Result<Self, String> {
        match AnyModel::raw_type(&value) {
            Some("chat") => serde_json::from_value(value).map(AnyModel::Chat).map_err(|e| e.to_string()),
            Some("image") => serde_json::from_value(value).map(AnyModel::Image).map_err(|e| e.to_string()),
            Some("classifier") => {
                serde_json::from_value(value).map(AnyModel::Classifier).map_err(|e| e.to_string())
            }
            Some(other) => Err(format!("Unknown model type: {other}")),
            None => Err("Invalid model type".to_string()),
        }
    }
}
