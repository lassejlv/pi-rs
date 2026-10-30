//! Stateful agent wrapper around the low-level loop (port of `agent.ts`).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use indexmap::IndexSet;
use parking_lot::Mutex;
use pi_ai::utils::now_ms;
use pi_ai::utils::transcript::create_initial_system_message;
use pi_ai::{
    AssistantContent, AssistantMessage, ClientIdentity, ImageContent, Model, ModelCost, OnPayload,
    OnProviderStreamEvent, OnResponse, SimpleStreamOptions, StopReason, TextContent,
    ThinkingBudgets, Transport, Usage, UserContent, UserContentBlock, UserMessage,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentEventSink, run_agent_loop, run_agent_loop_continue};
use crate::error::AgentError;
use crate::stream_fn::get_default_stream_fn;
use crate::transcript::get_current_system_message;
use crate::types::{
    AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate, AgentMessage,
    AgentState, AgentToolRef, BeforeToolCallFn, ConvertToLlmFn, FinishTurnFn, GetApiKeyFn,
    PrepareNextTurnContext, PrepareRequestFn, QueueMode, StreamFn, ThinkingLevel,
    ToolExecutionMode, TransformContextFn, default_convert_to_llm_fn, to_tool_declaration,
};

/// Listener for agent events. Listener futures are awaited in subscription order and are
/// part of the current run's settlement. They also receive the run's abort signal.
pub type AgentListener =
    Arc<dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

/// Legacy `prepareNextTurn` callback that only receives the abort signal.
pub type AgentPrepareNextTurnFn =
    Arc<dyn Fn(CancellationToken) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync>;

/// `prepareNextTurnWithContext` callback.
pub type AgentPrepareNextTurnWithContextFn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

/// The model used when none is configured.
pub fn default_model() -> Model {
    Model {
        id: "unknown".into(),
        name: "unknown".into(),
        api: "unknown".into(),
        provider: "unknown".into(),
        base_url: String::new(),
        reasoning: false,
        input: Vec::new(),
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        ..Default::default()
    }
}

/// Initial state for [`Agent`]. `system_prompt` and `tools` become the leading system message
/// unless `messages` already starts with one.
#[derive(Clone, Default)]
pub struct AgentInitialState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Option<Vec<AgentToolRef>>,
    pub messages: Option<Vec<AgentMessage>>,
}

fn create_agent_state(initial_state: Option<AgentInitialState>) -> AgentState {
    let initial_state = initial_state.unwrap_or_default();
    let tools = initial_state.tools.unwrap_or_default();
    let mut messages = initial_state.messages.unwrap_or_default();
    let declarations: Vec<pi_ai::Tool> = tools
        .iter()
        .map(|t| to_tool_declaration(t.as_ref()))
        .collect();
    let initial_message =
        create_initial_system_message(initial_state.system_prompt.as_deref(), Some(&declarations));
    if !matches!(messages.first(), Some(AgentMessage::System(_)))
        && let Some(initial_message) = initial_message
    {
        messages.insert(0, AgentMessage::System(initial_message));
    }
    AgentState {
        model: initial_state.model.unwrap_or_else(default_model),
        thinking_level: initial_state.thinking_level.unwrap_or(ThinkingLevel::Off),
        tools,
        messages,
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: IndexSet::new(),
        error_message: None,
    }
}

/// Options for constructing an [`Agent`].
#[derive(Clone, Default)]
pub struct AgentOptions {
    pub initial_state: Option<AgentInitialState>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    /// Stream function; falls back to the configured default (`set_default_stream_fn`).
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub on_payload: Option<OnPayload>,
    pub on_response: Option<OnResponse>,
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub finish_turn: Option<FinishTurnFn>,
    pub prepare_request: Option<PrepareRequestFn>,
    pub prepare_next_turn: Option<AgentPrepareNextTurnFn>,
    pub prepare_next_turn_with_context: Option<AgentPrepareNextTurnWithContextFn>,
    /// Default: one-at-a-time.
    pub steering_mode: Option<QueueMode>,
    /// Default: one-at-a-time.
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Default: auto.
    pub transport: Option<Transport>,
    pub max_retry_delay_ms: Option<u64>,
    /// Default: parallel.
    pub tool_execution: Option<ToolExecutionMode>,
    /// How requests identify the app to providers, merged over the default agent identity
    /// (`{ attribution: pi }`). Use `attribution: Some(None)` to send no attribution headers.
    pub identity: Option<ClientIdentity>,
}

/// Mutable runtime configuration of an [`Agent`] (the public fields of the TypeScript class).
#[derive(Clone)]
pub struct AgentSettings {
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub on_payload: Option<OnPayload>,
    pub on_response: Option<OnResponse>,
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub finish_turn: Option<FinishTurnFn>,
    pub prepare_request: Option<PrepareRequestFn>,
    pub prepare_next_turn: Option<AgentPrepareNextTurnFn>,
    pub prepare_next_turn_with_context: Option<AgentPrepareNextTurnWithContextFn>,
    /// Session identifier forwarded to providers for cache-aware backends.
    pub session_id: Option<String>,
    /// Optional per-level thinking token budgets forwarded to the stream function.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Preferred transport forwarded to the stream function.
    pub transport: Transport,
    /// Optional cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool execution strategy for assistant messages with multiple tool calls.
    pub tool_execution: ToolExecutionMode,
    /// Request identity forwarded to the stream function, over the default agent identity.
    pub identity: Option<ClientIdentity>,
}

struct PendingMessageQueue {
    messages: VecDeque<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: VecDeque::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push_back(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.iter().cloned().collect(),
            QueueMode::OneAtATime | QueueMode::Other(_) => self.messages.front().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime | QueueMode::Other(_) => self.messages.pop_front().into_iter().collect(),
        }
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

struct ActiveRun {
    abort: CancellationToken,
    idle: watch::Receiver<bool>,
}

struct AgentInner {
    state: Mutex<AgentState>,
    listeners: Mutex<Vec<(u64, AgentListener)>>,
    next_listener_id: AtomicU64,
    steering_queue: Mutex<PendingMessageQueue>,
    follow_up_queue: Mutex<PendingMessageQueue>,
    settings: Mutex<AgentSettings>,
    active_run: Mutex<Option<ActiveRun>>,
    /// Replacement set by a listener of the `message_start`/`message_end` event being dispatched.
    listener_replacement: Mutex<Option<AgentMessage>>,
    /// The final replacement of the last dispatched message event, for the loop to take.
    loop_replacement: Mutex<Option<AgentMessage>>,
}

/// Input accepted by [`Agent::prompt`]: text with optional images, or messages.
#[derive(Debug, Clone)]
pub enum PromptInput {
    Text {
        text: String,
        images: Vec<ImageContent>,
    },
    Messages(Vec<AgentMessage>),
}

impl PromptInput {
    pub fn text_with_images(text: impl Into<String>, images: Vec<ImageContent>) -> Self {
        PromptInput::Text {
            text: text.into(),
            images,
        }
    }
}

impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        PromptInput::Text {
            text: value.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        PromptInput::Text {
            text: value,
            images: Vec::new(),
        }
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(value: AgentMessage) -> Self {
        PromptInput::Messages(vec![value])
    }
}

impl From<UserMessage> for PromptInput {
    fn from(value: UserMessage) -> Self {
        PromptInput::Messages(vec![AgentMessage::User(value)])
    }
}

impl From<Vec<AgentMessage>> for PromptInput {
    fn from(value: Vec<AgentMessage>) -> Self {
        PromptInput::Messages(value)
    }
}

/// Handle returned by [`Agent::subscribe`].
pub struct Subscription {
    id: u64,
    inner: Weak<AgentInner>,
}

impl Subscription {
    /// Remove the listener.
    pub fn unsubscribe(self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.listeners.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

/// A weak handle to an [`Agent`], for listeners and hooks that must not keep it alive.
#[derive(Clone)]
pub struct WeakAgent {
    inner: Weak<AgentInner>,
}

impl WeakAgent {
    pub fn upgrade(&self) -> Option<Agent> {
        self.inner.upgrade().map(|inner| Agent { inner })
    }
}

/// Stateful wrapper around the low-level agent loop.
///
/// `Agent` owns the current transcript, emits lifecycle events, executes tools, and exposes
/// queueing APIs for steering and follow-up messages. Cloning yields another handle to the
/// same agent. Runs execute on the tokio runtime.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

macro_rules! setting_accessors {
    ($($(#[$doc:meta])* $field:ident, $setter:ident: $ty:ty;)*) => {
        $(
            $(#[$doc])*
            pub fn $field(&self) -> $ty {
                self.inner.settings.lock().$field.clone()
            }

            $(#[$doc])*
            pub fn $setter(&self, value: $ty) {
                self.inner.settings.lock().$field = value;
            }
        )*
    };
}

impl Agent {
    pub fn new(options: AgentOptions) -> Self {
        let settings = AgentSettings {
            convert_to_llm: options
                .convert_to_llm
                .unwrap_or_else(default_convert_to_llm_fn),
            transform_context: options.transform_context,
            stream_fn: options.stream_fn.or_else(|| get_default_stream_fn().ok()),
            get_api_key: options.get_api_key,
            on_payload: options.on_payload,
            on_response: options.on_response,
            on_provider_stream_event: options.on_provider_stream_event,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            finish_turn: options.finish_turn,
            prepare_request: options.prepare_request,
            prepare_next_turn: options.prepare_next_turn,
            prepare_next_turn_with_context: options.prepare_next_turn_with_context,
            session_id: options.session_id,
            thinking_budgets: options.thinking_budgets,
            transport: options.transport.unwrap_or(Transport::Auto),
            max_retry_delay_ms: options.max_retry_delay_ms,
            tool_execution: options
                .tool_execution
                .unwrap_or(ToolExecutionMode::Parallel),
            identity: options.identity,
        };
        Self {
            inner: Arc::new(AgentInner {
                state: Mutex::new(create_agent_state(options.initial_state)),
                listeners: Mutex::new(Vec::new()),
                next_listener_id: AtomicU64::new(1),
                steering_queue: Mutex::new(PendingMessageQueue::new(
                    options.steering_mode.unwrap_or_default(),
                )),
                follow_up_queue: Mutex::new(PendingMessageQueue::new(
                    options.follow_up_mode.unwrap_or_default(),
                )),
                settings: Mutex::new(settings),
                active_run: Mutex::new(None),
                listener_replacement: Mutex::new(None),
                loop_replacement: Mutex::new(None),
            }),
        }
    }

    pub fn downgrade(&self) -> WeakAgent {
        WeakAgent {
            inner: Arc::downgrade(&self.inner),
        }
    }

    // -----------------------------------------------------------------------
    // Listeners
    // -----------------------------------------------------------------------

    /// Subscribe to agent lifecycle events.
    ///
    /// Listener futures are awaited in subscription order and are included in the current
    /// run's settlement. `agent_end` is the final event of a run, but the agent does not
    /// become idle until all listeners for that event have settled.
    pub fn subscribe(&self, listener: AgentListener) -> Subscription {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::SeqCst);
        self.inner.listeners.lock().push((id, listener));
        Subscription {
            id,
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Subscribe with a synchronous listener.
    pub fn subscribe_fn(
        &self,
        listener: impl Fn(&AgentEvent, &CancellationToken) + Send + Sync + 'static,
    ) -> Subscription {
        self.subscribe(Arc::new(move |event, signal| {
            listener(&event, &signal);
            Box::pin(async {})
        }))
    }

    // -----------------------------------------------------------------------
    // State
    // -----------------------------------------------------------------------

    /// A snapshot of the current state.
    pub fn state(&self) -> AgentState {
        self.inner.state.lock().clone()
    }

    /// Read the state without cloning it.
    pub fn with_state<R>(&self, f: impl FnOnce(&AgentState) -> R) -> R {
        f(&self.inner.state.lock())
    }

    /// Replace the message of the `message_start`/`message_end` event currently being dispatched
    /// to listeners. Call it from a listener of that event. TypeScript listeners mutate the shared
    /// message object in place; this applies the same change: later listeners of the event, the
    /// message in agent state (for `message_end`) and the agent loop's own copy see the
    /// replacement, so tool execution, `turn_end`/`agent_end` and the turn context use it too.
    /// Ignored outside such a dispatch.
    pub fn replace_event_message(&self, message: AgentMessage) {
        *self.inner.listener_replacement.lock() = Some(message);
    }

    /// Mutate the state in place (for example, to append a message).
    pub fn with_state_mut<R>(&self, f: impl FnOnce(&mut AgentState) -> R) -> R {
        f(&mut self.inner.state.lock())
    }

    /// Current system prompt, replayed from the transcript's system messages.
    pub fn system_prompt(&self) -> String {
        self.with_state(AgentState::system_prompt)
    }

    pub fn model(&self) -> Model {
        self.with_state(|s| s.model.clone())
    }

    pub fn set_model(&self, model: Model) {
        self.with_state_mut(|s| s.model = model);
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.with_state(|s| s.thinking_level)
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.with_state_mut(|s| s.thinking_level = level);
    }

    pub fn tools(&self) -> Vec<AgentToolRef> {
        self.with_state(|s| s.tools.clone())
    }

    /// Replace the executable tools. Differences from the transcript's declared tools are
    /// announced to the model before the next request.
    pub fn set_tools(&self, tools: Vec<AgentToolRef>) {
        // Drop the previous tools after releasing the state lock: a tool's destructor may run
        // arbitrary code (e.g. release an extension handle that reaches back into the agent).
        let previous = self.with_state_mut(|s| std::mem::replace(&mut s.tools, tools));
        drop(previous);
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.with_state(|s| s.messages.clone())
    }

    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.with_state_mut(|s| s.messages = messages);
    }

    pub fn is_streaming(&self) -> bool {
        self.with_state(|s| s.is_streaming)
    }

    pub fn streaming_message(&self) -> Option<AgentMessage> {
        self.with_state(|s| s.streaming_message.clone())
    }

    pub fn pending_tool_calls(&self) -> IndexSet<String> {
        self.with_state(|s| s.pending_tool_calls.clone())
    }

    pub fn error_message(&self) -> Option<String> {
        self.with_state(|s| s.error_message.clone())
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    /// A copy of the runtime configuration.
    pub fn settings(&self) -> AgentSettings {
        self.inner.settings.lock().clone()
    }

    /// Mutate the runtime configuration.
    pub fn update_settings<R>(&self, f: impl FnOnce(&mut AgentSettings) -> R) -> R {
        f(&mut self.inner.settings.lock())
    }

    setting_accessors! {
        convert_to_llm, set_convert_to_llm: ConvertToLlmFn;
        transform_context, set_transform_context: Option<TransformContextFn>;
        stream_fn, set_stream_fn: Option<StreamFn>;
        get_api_key, set_get_api_key: Option<GetApiKeyFn>;
        on_payload, set_on_payload: Option<OnPayload>;
        on_response, set_on_response: Option<OnResponse>;
        on_provider_stream_event, set_on_provider_stream_event: Option<OnProviderStreamEvent>;
        before_tool_call, set_before_tool_call: Option<BeforeToolCallFn>;
        after_tool_call, set_after_tool_call: Option<AfterToolCallFn>;
        finish_turn, set_finish_turn: Option<FinishTurnFn>;
        prepare_request, set_prepare_request: Option<PrepareRequestFn>;
        prepare_next_turn, set_prepare_next_turn: Option<AgentPrepareNextTurnFn>;
        prepare_next_turn_with_context, set_prepare_next_turn_with_context: Option<AgentPrepareNextTurnWithContextFn>;
        session_id, set_session_id: Option<String>;
        thinking_budgets, set_thinking_budgets: Option<ThinkingBudgets>;
        transport, set_transport: Transport;
        max_retry_delay_ms, set_max_retry_delay_ms: Option<u64>;
        tool_execution, set_tool_execution: ToolExecutionMode;
        identity, set_identity: Option<ClientIdentity>;
    }

    // -----------------------------------------------------------------------
    // Queues
    // -----------------------------------------------------------------------

    /// How queued steering messages are drained.
    pub fn steering_mode(&self) -> QueueMode {
        self.inner.steering_queue.lock().mode.clone()
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.steering_queue.lock().mode = mode;
    }

    /// How queued follow-up messages are drained.
    pub fn follow_up_mode(&self) -> QueueMode {
        self.inner.follow_up_queue.lock().mode.clone()
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.follow_up_queue.lock().mode = mode;
    }

    /// Queue a message to be injected after the current assistant turn finishes.
    pub fn steer(&self, message: impl Into<AgentMessage>) {
        self.inner.steering_queue.lock().enqueue(message.into());
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&self, message: impl Into<AgentMessage>) {
        self.inner.follow_up_queue.lock().enqueue(message.into());
    }

    pub fn clear_steering_queue(&self) {
        self.inner.steering_queue.lock().clear();
    }

    pub fn clear_follow_up_queue(&self) {
        self.inner.follow_up_queue.lock().clear();
    }

    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// True when either queue still contains pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.inner.steering_queue.lock().has_items()
            || self.inner.follow_up_queue.lock().has_items()
    }

    /// Preview the messages selected for the next turn without consuming them.
    pub fn peek_queued_messages(&self) -> Vec<AgentMessage> {
        let steering = self.inner.steering_queue.lock().peek();
        if !steering.is_empty() {
            return steering;
        }
        self.inner.follow_up_queue.lock().peek()
    }

    // -----------------------------------------------------------------------
    // Run control
    // -----------------------------------------------------------------------

    /// Active abort signal for the current run, if any.
    pub fn signal(&self) -> Option<CancellationToken> {
        self.inner
            .active_run
            .lock()
            .as_ref()
            .map(|run| run.abort.clone())
    }

    /// Abort the current run, if one is active.
    pub fn abort(&self) {
        if let Some(run) = self.inner.active_run.lock().as_ref() {
            run.abort.cancel();
        }
    }

    /// Resolve when the current run and all awaited event listeners have finished.
    pub fn wait_for_idle(&self) -> impl Future<Output = ()> + Send + 'static {
        let idle = self
            .inner
            .active_run
            .lock()
            .as_ref()
            .map(|run| run.idle.clone());
        async move {
            if let Some(mut idle) = idle {
                let _ = idle.wait_for(|done| *done).await;
            }
        }
    }

    /// Clear conversation state and queues while retaining the replayed prompt/tool baseline.
    pub fn reset(&self) -> Result<(), AgentError> {
        if self.inner.active_run.lock().is_some() {
            return Err(AgentError::new(
                "Agent is already processing. Wait for completion before resetting.",
            ));
        }
        {
            let mut state = self.inner.state.lock();
            let baseline = get_current_system_message(&state.messages);
            state.messages = baseline.map(AgentMessage::System).into_iter().collect();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = IndexSet::new();
            state.error_message = None;
        }
        self.clear_follow_up_queue();
        self.clear_steering_queue();
        Ok(())
    }

    /// Start a new prompt from text (with optional images), a single message, or messages.
    ///
    /// The run starts immediately (the returned future only waits for it), so dropping the
    /// future does not cancel the run. Must be called within a tokio runtime.
    pub fn prompt(
        &self,
        input: impl Into<PromptInput>,
    ) -> impl Future<Output = Result<(), AgentError>> + Send + 'static {
        let started = if self.inner.active_run.lock().is_some() {
            Err(AgentError::new(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion.",
            ))
        } else {
            let messages = normalize_prompt_input(input.into());
            self.start_run(RunKind::Prompt {
                messages,
                skip_initial_steering_poll: false,
            })
        };
        await_run(started)
    }

    /// Continue from the current transcript. The last message must be a user or tool-result
    /// message; with an assistant tail, queued steering or follow-up messages are run instead.
    pub fn continue_run(&self) -> impl Future<Output = Result<(), AgentError>> + Send + 'static {
        await_run(self.start_continue())
    }

    fn start_continue(&self) -> Result<RunHandle, AgentError> {
        if self.inner.active_run.lock().is_some() {
            return Err(AgentError::new(
                "Agent is already processing. Wait for completion before continuing.",
            ));
        }

        let last_is_assistant = {
            let state = self.inner.state.lock();
            let Some(last_message) = state.messages.last() else {
                return Err(AgentError::new("No messages to continue from"));
            };
            if state
                .messages
                .iter()
                .all(|m| matches!(m, AgentMessage::System(_)))
            {
                return Err(AgentError::new("No messages to continue from"));
            }
            matches!(last_message, AgentMessage::Assistant(_))
        };

        if last_is_assistant {
            let queued_steering = self.inner.steering_queue.lock().drain();
            if !queued_steering.is_empty() {
                return self.start_run(RunKind::Prompt {
                    messages: queued_steering,
                    skip_initial_steering_poll: true,
                });
            }

            let queued_follow_ups = self.inner.follow_up_queue.lock().drain();
            if !queued_follow_ups.is_empty() {
                return self.start_run(RunKind::Prompt {
                    messages: queued_follow_ups,
                    skip_initial_steering_poll: false,
                });
            }

            return Err(AgentError::new(
                "Cannot continue from message role: assistant",
            ));
        }

        self.start_run(RunKind::Continue)
    }

    fn create_context_snapshot(&self) -> AgentContext {
        let state = self.inner.state.lock();
        AgentContext {
            messages: state.messages.clone(),
            tools: Some(state.tools.clone()),
        }
    }

    fn create_loop_config(&self, skip_initial_steering_poll: bool) -> AgentLoopConfig {
        let settings = self.settings();
        let (model, thinking_level) = self.with_state(|s| (s.model.clone(), s.thinking_level));
        let options = SimpleStreamOptions {
            reasoning: thinking_level.thinking(),
            session_id: settings.session_id.clone(),
            on_payload: settings.on_payload.clone(),
            on_response: settings.on_response.clone(),
            on_provider_stream_event: settings.on_provider_stream_event.clone(),
            transport: Some(settings.transport),
            thinking_budgets: settings.thinking_budgets.clone(),
            max_retry_delay_ms: settings.max_retry_delay_ms,
            identity: settings.identity.clone(),
            ..Default::default()
        };

        let prepare_next_turn: Option<crate::types::PrepareNextTurnFn> =
            if settings.prepare_next_turn_with_context.is_some()
                || settings.prepare_next_turn.is_some()
            {
                let weak = self.downgrade();
                let with_context = settings.prepare_next_turn_with_context.clone();
                let legacy = settings.prepare_next_turn.clone();
                Some(Arc::new(move |context| {
                    let signal = weak
                        .upgrade()
                        .and_then(|agent| agent.signal())
                        .unwrap_or_default();
                    let with_context = with_context.clone();
                    let legacy = legacy.clone();
                    Box::pin(async move {
                        if let Some(with_context) = with_context {
                            return with_context(context, signal).await;
                        }
                        match legacy {
                            Some(legacy) => legacy(signal).await,
                            None => None,
                        }
                    })
                }))
            } else {
                None
            };

        let skip_flag = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        let steering_inner = Arc::downgrade(&self.inner);
        let follow_up_inner = Arc::downgrade(&self.inner);
        let replacement_inner = Arc::downgrade(&self.inner);

        AgentLoopConfig {
            model,
            options,
            convert_to_llm: settings.convert_to_llm,
            transform_context: settings.transform_context,
            get_api_key: settings.get_api_key,
            finish_turn: settings.finish_turn,
            prepare_request: settings.prepare_request,
            prepare_next_turn,
            get_steering_messages: Some(Arc::new(move || {
                let messages = if skip_flag.swap(false, Ordering::SeqCst) {
                    Vec::new()
                } else {
                    steering_inner
                        .upgrade()
                        .map(|inner| inner.steering_queue.lock().drain())
                        .unwrap_or_default()
                };
                Box::pin(async move { messages })
            })),
            get_follow_up_messages: Some(Arc::new(move || {
                let messages = follow_up_inner
                    .upgrade()
                    .map(|inner| inner.follow_up_queue.lock().drain())
                    .unwrap_or_default();
                Box::pin(async move { messages })
            })),
            tool_execution: Some(settings.tool_execution),
            before_tool_call: settings.before_tool_call,
            after_tool_call: settings.after_tool_call,
            take_message_replacement: Some(Arc::new(move || {
                replacement_inner
                    .upgrade()
                    .and_then(|inner| inner.loop_replacement.lock().take())
            })),
        }
    }

    /// Start a run synchronously: mark the agent busy and spawn the loop.
    fn start_run(&self, kind: RunKind) -> Result<RunHandle, AgentError> {
        let abort = CancellationToken::new();
        let (idle_tx, idle_rx) = watch::channel(false);
        {
            let mut active_run = self.inner.active_run.lock();
            if active_run.is_some() {
                return Err(AgentError::new("Agent is already processing."));
            }
            *active_run = Some(ActiveRun {
                abort: abort.clone(),
                idle: idle_rx,
            });
        }
        {
            let mut state = self.inner.state.lock();
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        }

        let context = self.create_context_snapshot();
        let skip_initial_steering_poll = matches!(
            kind,
            RunKind::Prompt {
                skip_initial_steering_poll: true,
                ..
            }
        );
        let config = self.create_loop_config(skip_initial_steering_poll);
        let stream_fn = self.inner.settings.lock().stream_fn.clone();
        let inner = self.inner.clone();

        let handle = tokio::spawn(async move {
            let _finish = FinishRunGuard {
                inner: inner.clone(),
                idle_tx,
            };
            let emit_inner = inner.clone();
            let emit_signal = abort.clone();
            let emit: AgentEventSink = Arc::new(move |event| {
                let inner = emit_inner.clone();
                let signal = emit_signal.clone();
                Box::pin(async move { process_events(&inner, event, &signal).await })
            });
            let stream_fn = match stream_fn {
                Some(stream_fn) => Ok(stream_fn),
                None => get_default_stream_fn(),
            };
            let result = match stream_fn {
                Err(error) => Err(anyhow::Error::new(error)),
                Ok(stream_fn) => match kind {
                    RunKind::Prompt { messages, .. } => run_agent_loop(
                        messages,
                        context,
                        config,
                        emit,
                        Some(abort.clone()),
                        Some(stream_fn),
                    )
                    .await
                    .map(|_| ()),
                    RunKind::Continue => run_agent_loop_continue(
                        context,
                        config,
                        emit,
                        Some(abort.clone()),
                        Some(stream_fn),
                    )
                    .await
                    .map(|_| ()),
                },
            };
            if let Err(error) = result {
                handle_run_failure(&inner, error.to_string(), &abort).await;
            }
        });
        Ok(handle)
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("state", &*self.inner.state.lock())
            .finish_non_exhaustive()
    }
}

enum RunKind {
    Prompt {
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    },
    Continue,
}

type RunHandle = tokio::task::JoinHandle<()>;

async fn await_run(started: Result<RunHandle, AgentError>) -> Result<(), AgentError> {
    let handle = started?;
    handle
        .await
        .map_err(|error| AgentError::new(format!("Agent run failed: {error}")))
}

fn normalize_prompt_input(input: PromptInput) -> Vec<AgentMessage> {
    match input {
        PromptInput::Messages(messages) => messages,
        PromptInput::Text { text, images } => {
            let mut content = vec![UserContentBlock::Text(TextContent::new(text))];
            content.extend(images.into_iter().map(UserContentBlock::Image));
            vec![AgentMessage::User(UserMessage {
                content: UserContent::Blocks(content),
                timestamp: now_ms(),
                ..Default::default()
            })]
        }
    }
}

/// Clears runtime-owned state when a run settles (also on panic).
struct FinishRunGuard {
    inner: Arc<AgentInner>,
    idle_tx: watch::Sender<bool>,
}

impl Drop for FinishRunGuard {
    fn drop(&mut self) {
        {
            let mut state = self.inner.state.lock();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = IndexSet::new();
        }
        self.inner.active_run.lock().take();
        let _ = self.idle_tx.send(true);
    }
}

async fn handle_run_failure(
    inner: &Arc<AgentInner>,
    error_message: String,
    signal: &CancellationToken,
) {
    let model = inner.state.lock().model.clone();
    let failure_message = AgentMessage::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent::new(""))],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: if signal.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        },
        error_message: Some(error_message),
        timestamp: now_ms(),
        ..Default::default()
    });
    process_events(
        inner,
        AgentEvent::MessageStart {
            message: failure_message.clone(),
        },
        signal,
    )
    .await;
    process_events(
        inner,
        AgentEvent::MessageEnd {
            message: failure_message.clone(),
        },
        signal,
    )
    .await;
    process_events(
        inner,
        AgentEvent::TurnEnd {
            message: failure_message.clone(),
            tool_results: Vec::new(),
        },
        signal,
    )
    .await;
    process_events(
        inner,
        AgentEvent::AgentEnd {
            messages: vec![failure_message],
        },
        signal,
    )
    .await;
}

/// Reduce internal state for a loop event, then await listeners.
async fn process_events(inner: &Arc<AgentInner>, mut event: AgentEvent, signal: &CancellationToken) {
    let message_event = matches!(
        event,
        AgentEvent::MessageStart { .. } | AgentEvent::MessageEnd { .. }
    );
    // Index of the message_end message in state, for listener replacements.
    let mut state_index = None;
    {
        let mut state = inner.state.lock();
        if let AgentEvent::MessageEnd { .. } = &event {
            state_index = Some(state.messages.len());
        }
        match &event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
                state.streaming_message = Some(message.clone());
            }
            AgentEvent::MessageEnd { message } => {
                state.streaming_message = None;
                state.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                state.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                state.pending_tool_calls.shift_remove(tool_call_id);
            }
            AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(message),
                ..
            } => {
                if let Some(error_message) =
                    message.error_message.as_ref().filter(|m| !m.is_empty())
                {
                    state.error_message = Some(error_message.clone());
                }
            }
            AgentEvent::AgentEnd { .. } => {
                state.streaming_message = None;
            }
            _ => {}
        }
    }

    let listeners: Vec<AgentListener> = inner
        .listeners
        .lock()
        .iter()
        .map(|(_, l)| l.clone())
        .collect();
    if !message_event {
        for listener in listeners {
            listener(event.clone(), signal.clone()).await;
        }
        return;
    }

    inner.listener_replacement.lock().take();
    let mut replaced = None;
    for listener in listeners {
        listener(event.clone(), signal.clone()).await;
        let Some(replacement) = inner.listener_replacement.lock().take() else {
            continue;
        };
        match &mut event {
            AgentEvent::MessageStart { message } => {
                *message = replacement.clone();
                inner.state.lock().streaming_message = Some(replacement.clone());
            }
            AgentEvent::MessageEnd { message } => {
                *message = replacement.clone();
                if let Some(index) = state_index {
                    let mut state = inner.state.lock();
                    if let Some(slot) = state.messages.get_mut(index) {
                        *slot = replacement.clone();
                    }
                }
            }
            _ => {}
        }
        replaced = Some(replacement);
    }
    *inner.loop_replacement.lock() = replaced;
}
