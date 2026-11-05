//! Agent types (port of `types.ts`).
//!
//! `AgentMessage` is the union of pi-ai [`Message`]s and app-declared custom messages.
//! TypeScript extends the union through declaration merging; Rust models it as an enum
//! whose [`AgentMessage::Custom`] variant carries any other `role` with its raw JSON
//! fields, so custom kinds round-trip byte-for-byte.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexSet;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, ConstrainedSampling,
    JsonObject, Message, Model, SimpleStreamOptions, SystemMessage, Tool, ToolCall,
    ToolResultMessage, TranscriptContext, Usage, UserContentBlock, UserMessage,
};
use serde::de::{DeserializeOwned, Error as _};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Thinking/reasoning level, including `"off"`.
///
/// `"xhigh"` and `"max"` are only supported by selected model families.
pub type ThinkingLevel = pi_ai::ModelThinkingLevel;

/// A single tool call content block emitted by an assistant message.
pub type AgentToolCall = ToolCall;

// ---------------------------------------------------------------------------
// Stream function
// ---------------------------------------------------------------------------

/// Stream function used by the agent loop. pi-ai's `stream_simple` satisfies this shape.
///
/// The loop passes a normalized transcript: the system prompt and tool declarations are
/// carried by the transcript's system messages.
///
/// Contract: request/model/runtime failures must be encoded in the returned stream (a final
/// assistant message with stop reason `error` or `aborted`). Returning `Err` is treated like
/// a thrown error in TypeScript: the run fails and the agent records a failure message.
pub type StreamFn = Arc<
    dyn Fn(
            Model,
            TranscriptContext,
            SimpleStreamOptions,
        ) -> BoxFuture<'static, anyhow::Result<AssistantMessageEventStream>>
        + Send
        + Sync,
>;

/// Build a [`StreamFn`] from a closure returning a stream synchronously.
pub fn stream_fn<F>(f: F) -> StreamFn
where
    F: Fn(Model, TranscriptContext, SimpleStreamOptions) -> AssistantMessageEventStream
        + Send
        + Sync
        + 'static,
{
    Arc::new(move |model, context, options| {
        let stream = f(model, context, options);
        Box::pin(async move { Ok(stream) })
    })
}

// ---------------------------------------------------------------------------
// Execution modes
// ---------------------------------------------------------------------------

/// How tool calls from a single assistant message are executed.
///
/// - `Sequential`: each tool call is prepared, executed, and finalized before the next one starts.
/// - `Parallel`: tool calls are prepared sequentially, then allowed tools execute concurrently.
///   `tool_execution_end` is emitted in completion order, tool-result messages in source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// How many queued messages are injected at a queue drain point.
///
/// TS types this as `"all" | "one-at-a-time"` but stores whatever it is given (e.g. an
/// unrecognized `steeringMode` from settings.json) and hands it back unchanged; such values are
/// kept in [`QueueMode::Other`] and drain like `"one-at-a-time"` (TS only special-cases `"all"`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Drain every queued message.
    All,
    /// Drain only the oldest queued message.
    #[default]
    OneAtATime,
    /// Any other value (raw JSON), drained like [`QueueMode::OneAtATime`].
    Other(Value),
}

impl QueueMode {
    /// Map a JSON value to a mode, keeping unrecognized values verbatim.
    pub fn from_value(value: Value) -> Self {
        match value.as_str() {
            Some("all") => Self::All,
            Some("one-at-a-time") => Self::OneAtATime,
            _ => Self::Other(value),
        }
    }

    /// Map a string to a mode, keeping unrecognized strings verbatim.
    pub fn from_str_lossless(value: &str) -> Self {
        Self::from_value(Value::String(value.to_string()))
    }

    /// The JSON value TS would hold for this mode.
    pub fn to_value(&self) -> Value {
        match self {
            Self::All => Value::String("all".into()),
            Self::OneAtATime => Value::String("one-at-a-time".into()),
            Self::Other(value) => value.clone(),
        }
    }

    /// `String(mode)` in JS: the mode name, or the raw value's JS string form.
    pub fn to_js_string(&self) -> String {
        js_string(&self.to_value())
    }
}

/// `String(value)` for a JSON value, as JS computes it.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

impl Serialize for QueueMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for QueueMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(Value::deserialize(deserializer)?))
    }
}

/// Recovery policy for an effect whose durable intent exists but whose outcome is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolReplay {
    Never,
    Safe,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// An app-defined message kind (any `role` other than the four LLM roles).
///
/// `fields` holds every JSON field except `role`, in their original order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CustomMessage {
    pub role: String,
    pub fields: Map<String, Value>,
}

impl CustomMessage {
    pub fn new(role: impl Into<String>, fields: Map<String, Value>) -> Self {
        let mut fields = fields;
        fields.shift_remove("role");
        Self {
            role: role.into(),
            fields,
        }
    }

    /// Build from a typed value that serializes to an object with a `role` field.
    pub fn from_typed<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        match serde_json::to_value(value)? {
            Value::Object(mut map) => {
                let role = match map.shift_remove("role") {
                    Some(Value::String(role)) => role,
                    _ => {
                        return Err(serde_json::Error::custom(
                            "custom message must have a string role",
                        ));
                    }
                };
                Ok(Self { role, fields: map })
            }
            _ => Err(serde_json::Error::custom(
                "custom message must serialize to an object",
            )),
        }
    }

    /// Convert to a typed value (the `role` field is included in the input).
    pub fn to_typed<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.to_value())
    }

    /// JSON object including `role` first.
    pub fn to_value(&self) -> Value {
        let mut map = Map::with_capacity(self.fields.len() + 1);
        map.insert("role".to_string(), Value::String(self.role.clone()));
        for (key, value) in &self.fields {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// The `timestamp` field as milliseconds, or 0.
    pub fn timestamp(&self) -> i64 {
        match self.fields.get("timestamp") {
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .unwrap_or(0),
            _ => 0,
        }
    }
}

/// Union of LLM messages and custom app messages, discriminated by `role`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),
}

impl AgentMessage {
    pub fn role(&self) -> &str {
        match self {
            AgentMessage::System(_) => "system",
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::Custom(m) => &m.role,
        }
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            AgentMessage::System(m) => m.timestamp,
            AgentMessage::User(m) => m.timestamp,
            AgentMessage::Assistant(m) => m.timestamp,
            AgentMessage::ToolResult(m) => m.timestamp,
            AgentMessage::Custom(m) => m.timestamp(),
        }
    }

    /// A user message from text.
    pub fn user_text(text: impl Into<String>, timestamp: i64) -> Self {
        AgentMessage::User(UserMessage::new(text.into(), timestamp))
    }

    pub fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            AgentMessage::System(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_user(&self) -> Option<&UserMessage> {
        match self {
            AgentMessage::User(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            AgentMessage::Assistant(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_tool_result(&self) -> Option<&ToolResultMessage> {
        match self {
            AgentMessage::ToolResult(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_custom(&self) -> Option<&CustomMessage> {
        match self {
            AgentMessage::Custom(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, AgentMessage::Custom(_))
    }

    /// The LLM message, if this is not a custom message.
    pub fn to_message(&self) -> Option<Message> {
        match self {
            AgentMessage::System(m) => Some(Message::System(m.clone())),
            AgentMessage::User(m) => Some(Message::User(m.clone())),
            AgentMessage::Assistant(m) => Some(Message::Assistant(m.clone())),
            AgentMessage::ToolResult(m) => Some(Message::ToolResult(m.clone())),
            AgentMessage::Custom(_) => None,
        }
    }

    /// Convert into an LLM message, or return the custom message.
    pub fn into_message(self) -> Result<Message, CustomMessage> {
        match self {
            AgentMessage::System(m) => Ok(Message::System(m)),
            AgentMessage::User(m) => Ok(Message::User(m)),
            AgentMessage::Assistant(m) => Ok(Message::Assistant(m)),
            AgentMessage::ToolResult(m) => Ok(Message::ToolResult(m)),
            AgentMessage::Custom(m) => Err(m),
        }
    }

    /// Build a custom message from a typed value with a `role` field.
    pub fn custom<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        CustomMessage::from_typed(value).map(AgentMessage::Custom)
    }

    /// Parse any message JSON value.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

impl From<Message> for AgentMessage {
    fn from(value: Message) -> Self {
        match value {
            Message::System(m) => AgentMessage::System(m),
            Message::User(m) => AgentMessage::User(m),
            Message::Assistant(m) => AgentMessage::Assistant(m),
            Message::ToolResult(m) => AgentMessage::ToolResult(m),
        }
    }
}

impl From<SystemMessage> for AgentMessage {
    fn from(value: SystemMessage) -> Self {
        AgentMessage::System(value)
    }
}

impl From<UserMessage> for AgentMessage {
    fn from(value: UserMessage) -> Self {
        AgentMessage::User(value)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(value: AssistantMessage) -> Self {
        AgentMessage::Assistant(value)
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(value: ToolResultMessage) -> Self {
        AgentMessage::ToolResult(value)
    }
}

impl From<CustomMessage> for AgentMessage {
    fn from(value: CustomMessage) -> Self {
        AgentMessage::Custom(value)
    }
}

#[derive(Serialize)]
#[serde(tag = "role")]
enum LlmMessageRef<'a> {
    #[serde(rename = "system")]
    System(&'a SystemMessage),
    #[serde(rename = "user")]
    User(&'a UserMessage),
    #[serde(rename = "assistant")]
    Assistant(&'a AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(&'a ToolResultMessage),
}

impl Serialize for AgentMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AgentMessage::System(m) => LlmMessageRef::System(m).serialize(serializer),
            AgentMessage::User(m) => LlmMessageRef::User(m).serialize(serializer),
            AgentMessage::Assistant(m) => LlmMessageRef::Assistant(m).serialize(serializer),
            AgentMessage::ToolResult(m) => LlmMessageRef::ToolResult(m).serialize(serializer),
            AgentMessage::Custom(m) => {
                let mut map = serializer.serialize_map(Some(m.fields.len() + 1))?;
                map.serialize_entry("role", &m.role)?;
                for (key, value) in &m.fields {
                    if key != "role" {
                        map.serialize_entry(key, value)?;
                    }
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for AgentMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("message is missing a string `role`"))?
            .to_string();
        match role.as_str() {
            "system" | "user" | "assistant" | "toolResult" => {
                serde_json::from_value::<Message>(value)
                    .map(AgentMessage::from)
                    .map_err(D::Error::custom)
            }
            _ => match value {
                Value::Object(mut map) => {
                    map.shift_remove("role");
                    Ok(AgentMessage::Custom(CustomMessage { role, fields: map }))
                }
                _ => Err(D::Error::custom("message must be an object")),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Final or partial result produced by a tool.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    #[serde(default, deserialize_with = "pi_ai::serde_util::null_as_default")]
    pub content: Vec<UserContentBlock>,
    /// Arbitrary structured details for logs or UI rendering (`None` = undefined).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "pi_ai::serde_util::deserialize_some"
    )]
    pub details: Option<Value>,
    /// Machine-readable result matching the tool's `output_schema`. Not sent to the model.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "pi_ai::serde_util::deserialize_some"
    )]
    pub structured_content: Option<Value>,
    /// Usage from the tool execution itself, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Report a failure without returning an error. `details` and `structured_content` are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Hint that the agent should stop after the current tool batch (only when every result sets it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    /// A result with one text block and the given details.
    pub fn text(text: impl Into<String>, details: Option<Value>) -> Self {
        Self {
            content: vec![UserContentBlock::text(text)],
            details,
            ..Default::default()
        }
    }

    /// An error result: one text block and empty-object details.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![UserContentBlock::text(message)],
            details: Some(Value::Object(Map::new())),
            ..Default::default()
        }
    }
}

/// Callback used by tools to stream partial execution updates. Calls made after the
/// tool's `execute` future completes are ignored.
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Tool definition used by the agent runtime.
///
/// `name`, `description`, `parameters` (a JSON Schema) and `constrained_sampling` form the
/// pi-ai [`Tool`] declared to the model.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    /// Human-readable label for UI display.
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the parameters.
    fn parameters(&self) -> &Value;
    fn constrained_sampling(&self) -> Option<&ConstrainedSampling> {
        None
    }
    /// Optional compatibility shim for raw tool-call arguments before schema validation.
    /// Return `None` to keep the arguments unchanged.
    fn prepare_arguments(&self, _args: &JsonObject) -> Option<JsonObject> {
        None
    }
    /// JSON Schema of `structured_content` in successful results.
    fn output_schema(&self) -> Option<&Value> {
        None
    }
    /// Recovery policy for an effect whose outcome is unknown.
    fn replay(&self) -> Option<ToolReplay> {
        None
    }
    /// Per-tool execution mode override.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }
    /// The parameters were built with TypeBox (TS: the schema carries `~kind`), so validation runs
    /// TypeBox's `Value.Convert` on the arguments first (see [`Tool::typebox`]).
    fn parameters_typebox(&self) -> bool {
        false
    }
    /// Execute the tool call. Return `Err` on failure, or a result with `is_error: Some(true)`.
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult>;

    /// The tool declaration sent to the model.
    fn to_tool(&self) -> Tool {
        Tool {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters().clone(),
            constrained_sampling: self.constrained_sampling().cloned(),
            typebox: self.parameters_typebox(),
        }
    }
}

/// Shared handle to an [`AgentTool`].
pub type AgentToolRef = Arc<dyn AgentTool>;

/// The pi-ai declaration of a tool (`toToolDeclaration`).
pub fn to_tool_declaration(tool: &dyn AgentTool) -> Tool {
    pi_ai::utils::transcript::to_tool_declaration(&tool.to_tool())
}

type ExecuteFn = Arc<
    dyn Fn(
            String,
            Value,
            CancellationToken,
            AgentToolUpdateCallback,
        ) -> BoxFuture<'static, anyhow::Result<AgentToolResult>>
        + Send
        + Sync,
>;
type PrepareArgumentsFn = Arc<dyn Fn(&JsonObject) -> Option<JsonObject> + Send + Sync>;

/// A closure-backed [`AgentTool`].
#[derive(Clone)]
pub struct FnAgentTool {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Value,
    pub constrained_sampling: Option<ConstrainedSampling>,
    pub output_schema: Option<Value>,
    pub replay: Option<ToolReplay>,
    pub execution_mode: Option<ToolExecutionMode>,
    prepare_arguments: Option<PrepareArgumentsFn>,
    execute: ExecuteFn,
}

impl fmt::Debug for FnAgentTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnAgentTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .field("execution_mode", &self.execution_mode)
            .finish_non_exhaustive()
    }
}

impl FnAgentTool {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: F,
    ) -> Self
    where
        F: Fn(String, Value, CancellationToken, AgentToolUpdateCallback) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: std::future::Future<Output = anyhow::Result<AgentToolResult>> + Send + 'static,
    {
        Self {
            name: name.into(),
            label: label.into(),
            description: description.into(),
            parameters,
            constrained_sampling: None,
            output_schema: None,
            replay: None,
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(move |id, params, signal, on_update| {
                Box::pin(execute(id, params, signal, on_update))
            }),
        }
    }

    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub fn with_replay(mut self, replay: ToolReplay) -> Self {
        self.replay = Some(replay);
        self
    }

    pub fn with_constrained_sampling(mut self, constrained_sampling: ConstrainedSampling) -> Self {
        self.constrained_sampling = Some(constrained_sampling);
        self
    }

    pub fn with_prepare_arguments(
        mut self,
        prepare: impl Fn(&JsonObject) -> Option<JsonObject> + Send + Sync + 'static,
    ) -> Self {
        self.prepare_arguments = Some(Arc::new(prepare));
        self
    }

    /// Wrap in an [`AgentToolRef`].
    pub fn into_ref(self) -> AgentToolRef {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl AgentTool for FnAgentTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn constrained_sampling(&self) -> Option<&ConstrainedSampling> {
        self.constrained_sampling.as_ref()
    }
    fn prepare_arguments(&self, args: &JsonObject) -> Option<JsonObject> {
        self.prepare_arguments
            .as_ref()
            .and_then(|prepare| prepare(args))
    }
    fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }
    fn replay(&self) -> Option<ToolReplay> {
        self.replay
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: AgentToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        (self.execute)(tool_call_id.to_string(), params, signal, on_update).await
    }
}

/// Final outcome of a tool call after hooks ran.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentToolCallOutcome {
    pub tool_call: AgentToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
    /// Milliseconds `execute()` took (monotonic clock); `None` when the tool did not run.
    pub duration_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Context and state
// ---------------------------------------------------------------------------

/// Context snapshot passed into the low-level agent loop.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// Transcript visible to the model.
    pub messages: Vec<AgentMessage>,
    /// Tools available for execution in this run (`None` = undefined).
    pub tools: Option<Vec<AgentToolRef>>,
}

impl AgentContext {
    pub fn new(messages: Vec<AgentMessage>, tools: Vec<AgentToolRef>) -> Self {
        Self {
            messages,
            tools: Some(tools),
        }
    }

    pub fn tools(&self) -> &[AgentToolRef] {
        self.tools.as_deref().unwrap_or(&[])
    }
}

impl fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentContext")
            .field("messages", &self.messages)
            .field(
                "tools",
                &self.tools.as_ref().map(|tools| {
                    tools
                        .iter()
                        .map(|t| t.name().to_string())
                        .collect::<Vec<_>>()
                }),
            )
            .finish()
    }
}

/// Public agent state snapshot.
#[derive(Clone)]
pub struct AgentState {
    /// Active model used for future turns.
    pub model: Model,
    /// Requested reasoning level for future turns.
    pub thinking_level: ThinkingLevel,
    /// Executable tools. Differences from the transcript's declared tools are announced to the
    /// model with a system message before the next request.
    pub tools: Vec<AgentToolRef>,
    /// Conversation transcript; system messages carry the prompt and tool declarations.
    pub messages: Vec<AgentMessage>,
    /// True while the agent is processing a prompt or continuation (until awaited
    /// `agent_end` listeners settle).
    pub is_streaming: bool,
    /// Partial assistant message for the current streamed response, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: IndexSet<String>,
    /// Error message from the most recent failed or aborted assistant turn, if any.
    pub error_message: Option<String>,
}

impl AgentState {
    /// Current system prompt, replayed from the transcript's system messages.
    pub fn system_prompt(&self) -> String {
        crate::transcript::get_current_system_prompt(&self.messages)
    }
}

impl fmt::Debug for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentState")
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect::<Vec<_>>(),
            )
            .field("messages", &self.messages)
            .field("is_streaming", &self.is_streaming)
            .field("streaming_message", &self.streaming_message)
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Result returned from `before_tool_call`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeToolCallResult {
    /// Prevent the tool from executing; the loop emits an error tool result instead.
    pub block: bool,
    /// Text of the blocked error result. Defaults to "Tool execution was blocked".
    pub reason: Option<String>,
    /// Hint to stop after the current batch when blocked (only when every result sets it).
    pub terminate: Option<bool>,
    /// Replacement arguments for execution. This is the Rust equivalent of mutating
    /// `context.args` in place in TypeScript; they are not revalidated.
    pub args: Option<Value>,
}

/// Partial override returned from `after_tool_call`.
///
/// Field-by-field merge: provided fields replace the executed result's values. When
/// `content` is provided without `structured_content`, the structured content is dropped.
/// `Some(Value::Null)` in `details`/`structured_content` counts as not provided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContentBlock>>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// Context passed to `before_tool_call`.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    /// The assistant message that requested the tool call.
    pub assistant_message: AssistantMessage,
    /// The raw tool call block.
    pub tool_call: AgentToolCall,
    /// Validated tool arguments.
    pub args: Value,
    /// Current agent context at the time the tool call is prepared.
    pub context: AgentContext,
}

/// Context passed to `after_tool_call`.
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: Value,
    /// The executed tool result before any overrides are applied.
    pub result: AgentToolResult,
    /// Whether the executed result is currently treated as an error.
    pub is_error: bool,
    pub context: AgentContext,
}

/// Context passed to completed-turn callbacks.
#[derive(Debug, Clone)]
pub struct AgentTurnContext {
    /// The assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool result messages emitted for the completed turn.
    pub tool_results: Vec<ToolResultMessage>,
    /// Agent context after the turn's assistant message and tool results were appended.
    pub context: AgentContext,
    /// Messages this loop invocation will return if it exits at this point.
    pub new_messages: Vec<AgentMessage>,
}

/// Context passed to `prepare_next_turn`.
pub type PrepareNextTurnContext = AgentTurnContext;

/// Decision returned by `finish_turn`. `None` preserves normal scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum AgentTurnDecision {
    Continue,
    End,
}

/// Replacement runtime state used before starting another provider request.
#[derive(Debug, Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Context for the next provider request.
    pub context: Option<AgentContext>,
    /// Messages to append before the next provider request, with normal lifecycle events.
    pub messages: Option<Vec<AgentMessage>>,
    /// Model for the next provider request.
    pub model: Option<Model>,
    /// Thinking level for the next provider request.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Runtime state available immediately before a conversational provider request.
#[derive(Debug, Clone)]
pub struct PrepareRequestContext {
    pub context: AgentContext,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
}

/// Replacement runtime state for the provider request being prepared.
#[derive(Debug, Clone, Default)]
pub struct AgentRequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Converts `AgentMessage`s to LLM messages before each call. Must not fail.
pub type ConvertToLlmFn =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
/// Transform applied to the context before `convert_to_llm`. Must not fail.
pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;
/// Resolves an API key dynamically for each LLM call.
pub type GetApiKeyFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;
/// Called after a completed turn's tool results, immediately before `turn_end`.
pub type FinishTurnFn = Arc<
    dyn Fn(AgentTurnContext, CancellationToken) -> BoxFuture<'static, Option<AgentTurnDecision>>
        + Send
        + Sync,
>;
/// Called immediately before every conversational provider request. `Err` rejects the request:
/// the run ends with an error response (TypeScript `prepareRequest` throwing).
pub type PrepareRequestFn = Arc<
    dyn Fn(
            PrepareRequestContext,
            CancellationToken,
        ) -> BoxFuture<'static, anyhow::Result<Option<AgentRequestUpdate>>>
        + Send
        + Sync,
>;
/// Called after `turn_end` when the loop will continue, before the next turn starts.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync,
>;
/// Returns queued steering or follow-up messages.
pub type GetMessagesFn = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;
/// Takes the replacement a listener made for the message of the `message_start`/`message_end`
/// event that was just emitted (see [`crate::Agent::replace_event_message`]). TypeScript listeners
/// mutate the shared message object in place, so the loop's own copy (used for tool execution,
/// later events, the turn context and the returned messages) changes too; the loop calls this
/// after each such event and adopts the replacement where TypeScript shares the object.
pub type TakeMessageReplacementFn = Arc<dyn Fn() -> Option<AgentMessage> + Send + Sync>;
/// Called before a tool executes, after argument validation. `Err` becomes an error result.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, anyhow::Result<Option<BeforeToolCallResult>>>
        + Send
        + Sync,
>;
/// Called after a tool executes, before `tool_execution_end`. `Err` becomes an error result.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, anyhow::Result<Option<AfterToolCallResult>>>
        + Send
        + Sync,
>;

/// The default `convert_to_llm`: keeps system, user, assistant and tool-result messages.
pub fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(AgentMessage::to_message)
        .collect()
}

/// [`ConvertToLlmFn`] wrapping [`default_convert_to_llm`].
pub fn default_convert_to_llm_fn() -> ConvertToLlmFn {
    Arc::new(|messages| {
        let converted = default_convert_to_llm(&messages);
        Box::pin(async move { converted })
    })
}

/// Configuration of the low-level agent loop. `options` carries the stream options
/// (TypeScript's `AgentLoopConfig extends SimpleStreamOptions`).
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    /// Stream options forwarded to the stream function (`reasoning`, `session_id`, ...).
    pub options: SimpleStreamOptions,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub finish_turn: Option<FinishTurnFn>,
    pub prepare_request: Option<PrepareRequestFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    /// Tool execution mode. Default: parallel.
    pub tool_execution: Option<ToolExecutionMode>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Message replacements made by event listeners (see [`TakeMessageReplacementFn`]).
    pub take_message_replacement: Option<TakeMessageReplacementFn>,
}

impl AgentLoopConfig {
    /// A config for `model` with the default `convert_to_llm` and no hooks.
    pub fn new(model: Model) -> Self {
        Self {
            model,
            options: SimpleStreamOptions::default(),
            convert_to_llm: default_convert_to_llm_fn(),
            transform_context: None,
            get_api_key: None,
            finish_turn: None,
            prepare_request: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: None,
            before_tool_call: None,
            after_tool_call: None,
            take_message_replacement: None,
        }
    }
}

impl fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model.id)
            .field("options", &self.options)
            .field("tool_execution", &self.tool_execution)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events emitted by the agent for UI updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    /// Emitted for system, user, assistant, tool-result and custom messages.
    MessageStart {
        message: AgentMessage,
    },
    /// Only emitted for assistant messages during streaming.
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: Box<AssistantMessageEvent>,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

impl AgentEvent {
    /// The TypeScript `type` discriminant.
    pub fn type_name(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}
