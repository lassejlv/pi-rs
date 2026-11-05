//! Port of `packages/coding-agent/src/core/agent-session.ts`.

use crate::core::auth_guidance::{format_no_api_key_found_message, format_no_model_selected_message};
use crate::core::bash_executor::{BashExecutorOptions, BashResult, execute_bash_with_operations};
use crate::core::bug_report::{GenerateBugReportSummaryOptions, generate_bug_report_summary};
use crate::core::cache_warmer::{CacheWarmer, CacheWarmingStatus};
use crate::core::compaction::branch_summarization::{
    GenerateBranchSummaryOptions, collect_entries_for_branch_summary, generate_branch_summary,
};
use crate::core::compaction::compaction::{
    CompactionPreparation, CompactionResult, CompactionSettings, calculate_context_tokens, compact,
    estimate_context_tokens, estimate_projected_context_tokens, estimate_tokens,
    prepare_compaction, should_compact,
};
use crate::core::defaults::THINKING_LEVEL_OPTIONS;
use crate::core::export_html::tool_renderer::{ToolHtmlRendererDeps, create_tool_html_renderer};
use crate::core::export_html::{ExportOptions, ExportSessionToHtmlOptions, export_session_to_html};
use crate::core::extensions::runner::{
    BeforeAgentStartTrigger, BoundaryBaseEvent, BoundaryContextBuilder, DiscoveredResourcePath, ExtensionErrorListener,
    ExtensionRunner, ExtensionRunnerBindCoreProviderActions, SessionBeforeEventResult,
    ShutdownHandler, emit_session_shutdown_event, provider_config_input,
};
use crate::core::extensions::types::{
    AgentActivityOutcome, AgentEndEvent, AgentSettledEvent, AgentStartEvent, BeforeAgentStartKind,
    BoundaryContextPreview, CompactOptions, CompactionReason, ContextUsage, CustomMessageInput,
    DeliverAs, ExecuteToolFn, ExecuteToolOptions, ExtensionActions, ExtensionCommandContextActions,
    ExtensionContextActions, ExtensionError, ExtensionEvent, ExtensionMode, ExtensionUIContext,
    FlagValue, InputEventResult, InputSource, MessageEndEvent, MessageStartEvent,
    MessageUpdateEvent, ModelSelectEvent, ModelSelectSource, ProviderConfig, RegisteredTool,
    ReplacedSessionActions, ReplacedSessionContext, ResourcesDiscoverReason,
    SendMessageOptions, SendUserMessageOptions, SessionBeforeCompactEvent,
    SessionBeforeTreeEvent, SessionBeforeTreeSummary, SessionBoundaryDraft, SessionCompactEvent,
    SessionCompactFailedEvent, SessionInfoChangedEvent, SessionShutdownEvent,
    SessionShutdownReason, SessionStartEvent, SessionStartReason, SessionTreeEvent,
    StreamingBehavior, ThinkingLevelSelectEvent, ToolCallEvent, ToolDefinition,
    ToolExecutionEndEvent, ToolExecutionStartEvent, ToolExecutionUpdateEvent, ToolExposure,
    ToolInfo, ToolLoadout, ToolNamespace, ToolResultEvent, TreePreparation, TurnStartEvent,
};
use crate::core::extensions::wrapper::wrap_registered_tools;
use crate::core::mcp_servers::{create_tool_name_matcher, is_mcp_tool_name};
use crate::core::messages::{
    BASH_EXECUTION_ROLE, BashExecutionMessage, CUSTOM_ROLE, CustomMessage, convert_to_llm,
    parse_timestamp_ms,
};
use crate::core::model_registry::ModelRegistry;
use crate::core::model_resolver::ScopedModel;
use crate::core::model_runtime::{
    ModelRuntime, ModelRuntimeAuthOverrides, ModelRuntimeGetAuthProviderOrModel,
    ModelRuntimeResolveModelOptions,
};
use crate::core::nested_tool_calls::{
    NestedToolCallHost, NestedToolCallOptions, NestedToolCallRunner, NestedToolExecutionEvent,
    NestedUpdateSink,
};
use crate::core::package_manager::PathMetadata;
use crate::core::prompt_templates::{PromptTemplate, expand_prompt_template};
use crate::core::resource_loader::{
    ResourceExtensionPaths, ResourceExtensionPathsPromptPaths, ResourceExtensionPathsSkillPaths,
    ResourceExtensionPathsThemePaths, ResourceLoader,
};
use crate::core::session_export::export_session_to_jsonl;
use crate::core::session_manager::{
    BranchSummaryEntry, FileEntry, SessionEntry, SessionManager, SessionProjection, UsageEntry,
    get_latest_compaction_entry,
};
use crate::core::settings_manager::{
    DEFAULT_TOOL_NAMES, Settings, SettingsManager, SettingsManagerGetCompactionSettingsModel,
    apply_tool_modifiers,
};
use crate::core::settings_schema::CacheWarmingMode;
use crate::core::slash_commands::{SlashCommandInfo, SlashCommandSource};
use crate::core::source_info::{
    BUILTIN_PATH_PREFIX, CreateSyntheticSourceInfoOptions, SourceInfo, SourceOrigin, SourceScope,
    create_synthetic_source_info, is_synthetic_path,
};
use crate::core::system_prompt::{
    BuildSystemPromptOptions, BuildSystemPromptOptionsContextFiles,
    NormalizedBuildSystemPromptOptions, build_normalized_system_prompt_sections,
    build_system_prompt, diff_system_prompt_sections, normalize_build_system_prompt_options,
};
use crate::core::tools::bash::{
    BashOperations, BashToolOptions, CreateLocalBashOperationsOptions,
    create_local_bash_operations,
};
use crate::core::tools::read::ReadToolOptions;
use crate::core::tools::tool_definition_wrapper::create_tool_definition_from_agent_tool;
use crate::core::tools::{ToolsOptions, create_all_tool_definitions};
use crate::core::usage_totals::{add_usage_to_totals, combine_usage, create_usage_totals};
use crate::core::virtual_models::{
    ModelRouteReason, VIRTUAL_MODEL_STATE_ENTRY, VirtualModelDefinition, VirtualModelStateData,
    find_latest_response, get_branch_selection, get_virtual_model_state, is_virtual_model,
};
use crate::modes::interactive::theme::theme::{get_theme_by_name, theme};
use crate::utils::frontmatter::strip_frontmatter;
use crate::utils::image_process::{ProcessImageOptions, ProcessImageResult, process_image};
use crate::utils::tool_result_images::{
    NormalizeToolResultImagesOptions, normalize_tool_result_images,
};
use base64::Engine as _;
use futures::future::BoxFuture;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use pi_agent::agent::{Agent, AgentPrepareNextTurnWithContextFn};
use pi_agent::agent_loop::{RunToolCallOptions, run_tool_call};
use pi_agent::transcript::get_current_system_message;
use pi_agent::types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentLoopTurnUpdate,
    AgentMessage, AgentRequestUpdate, AgentState, AgentToolCall, AgentToolCallOutcome,
    AgentToolRef, AgentToolResult, AgentTurnDecision, BeforeToolCallContext, BeforeToolCallResult,
    PrepareNextTurnContext, PrepareRequestContext, PrepareRequestFn, QueueMode, ThinkingLevel,
    ToolExecutionMode,
};
use pi_ai::models::{chat_models_are_equal, clamp_thinking_level, get_supported_thinking_levels};
use pi_ai::types::{
    AnyModel, AssistantContent, AssistantMessage, AssistantMessageEvent, ImageContent, Message,
    Model, StopReason, SystemContent, SystemMessage, TextContent, ToolResultMessage, Usage,
    UserContent, UserContentBlock, UserMessage,
};
use pi_ai::utils::retry::{RetryCallbacks, RetryPolicy, retry_delay_ms};
use pi_ai::compat::reset_api_providers;
use pi_ai::{
    cleanup_session_resources, is_context_overflow, is_recoverable_length,
    is_retryable_assistant_error,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

/// Parsed skill block from a user message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message: Option<String>,
}

/// Parse a skill block from message text.
/// Returns null if the text doesn't contain a skill block.
pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    static SKILL_BLOCK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?s)\A<skill name="([^"]+)" location="([^"]+)">\n(.*?)\n</skill>(?:\n\n(.+))?\z"#,
        )
        .expect("valid regex")
    });
    let captures = SKILL_BLOCK.captures(text)?;
    Some(ParsedSkillBlock {
        name: captures[1].to_string(),
        location: captures[2].to_string(),
        content: captures[3].to_string(),
        user_message: captures
            .get(4)
            .map(|m| m.as_str().trim().to_string())
            .filter(|m| !m.is_empty()),
    })
}

/// Source of a `summarization_retry_attempt_start` event (discriminated by `source`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all_fields = "camelCase")]
pub enum SummarizationRetrySource {
    #[serde(rename = "branchSummary")]
    BranchSummary,
    #[serde(rename = "compaction")]
    Compaction { reason: CompactionReason },
}

/// Session-specific events that extend the core `AgentEvent` (TS `AgentSessionEvent`).
///
/// The core agent events are repeated here (except `agent_end`, which gains `willRetry`), and
/// the `tool_execution_*` events carry `parentToolCallId` for calls a tool made through
/// `ctx.executeTool()` (TS `WithParentToolCallId`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentSessionEvent {
    AgentStart,
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    AgentEnd {
        messages: Vec<AgentMessage>,
        will_retry: bool,
    },
    AgentSettled {
        aborted: bool,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    EntryAppended {
        entry: SessionEntry,
    },
    SessionInfoChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    CompactionEnd {
        reason: CompactionReason,
        /// `CompactionResult | undefined` (omitted when undefined).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart(SummarizationRetrySource),
    SummarizationRetryFinished,
    BashExecutionUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        delta: String,
    },
}

impl AgentSessionEvent {
    /// Map a core agent event (except `agent_end`, which the session re-emits with `will_retry`).
    pub fn from_agent_event(
        event: AgentEvent,
        parent_tool_call_id: Option<String>,
    ) -> Option<Self> {
        Some(match event {
            AgentEvent::AgentStart => Self::AgentStart,
            AgentEvent::AgentEnd { .. } => return None,
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => Self::TurnEnd {
                message,
                tool_results,
            },
            AgentEvent::MessageStart { message } => Self::MessageStart { message },
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => Self::MessageUpdate {
                message,
                assistant_message_event,
            },
            AgentEvent::MessageEnd { message } => Self::MessageEnd { message },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Self::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id,
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Self::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
                parent_tool_call_id,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
            } => Self::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
                parent_tool_call_id,
            },
        })
    }
}

pub type AgentSessionEventListener = Arc<dyn Fn(AgentSessionEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionConfigScopedModels {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

/// Mutable ref used by the agent's hooks (and the SDK) to reach the current ExtensionRunner
/// (TS `{ current?: ExtensionRunner }`). Cheap to clone; clones share the slot.
#[derive(Clone, Default)]
pub struct AgentSessionConfigExtensionRunnerRef {
    slot: Arc<parking_lot::RwLock<Option<Arc<ExtensionRunner>>>>,
}

impl AgentSessionConfigExtensionRunnerRef {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current runner, if one was created.
    pub fn current(&self) -> Option<Arc<ExtensionRunner>> {
        self.slot.read().clone()
    }

    pub fn set(&self, runner: Option<Arc<ExtensionRunner>>) {
        *self.slot.write() = runner;
    }
}

#[derive(Clone)]
pub struct AgentSessionConfig {
    pub agent: Agent,
    pub session_manager: Arc<SessionManager>,
    pub settings_manager: Arc<SettingsManager>,
    pub cwd: String,
    /// Models to cycle through with Ctrl+P (from --models flag)
    pub scoped_models: Option<Vec<AgentSessionConfigScopedModels>>,
    /// Resource loader for extensions, skills, prompts, themes, context files, and system prompt
    pub resource_loader: Arc<dyn ResourceLoader>,
    /// SDK custom tools registered outside extensions
    pub custom_tools: Option<Vec<ToolDefinition>>,
    /// Canonical model/auth runtime used by coding-agent internals.
    pub model_runtime: Arc<ModelRuntime>,
    /// Keeps the prompt cache entry of the last session request warm.
    pub cache_warmer: Option<Arc<CacheWarmer>>,
    /// Initial active built-in tool names. Default: [read, bash, edit, write]
    pub initial_active_tool_names: Option<Vec<String>>,
    /// Whether the initial tools come from the `defaultTools` setting. When true, a resumed session
    /// replaces them with the loadout its transcript records, with `defaultToolModifiers` applied, and
    /// reload activates tools newly added to the setting. Tools removed from it stay active.
    pub uses_default_tools: Option<bool>,
    /// `+name`/`-name` entries applied on top of the `defaultTools` setting, from `--tools`. Reload
    /// applies them to the reloaded setting too, so a removed tool stays removed.
    pub default_tool_modifiers: Option<Vec<String>>,
    /// Optional allowlist of tool names or patterns (`*` matches any characters). When provided, only
    /// matching tools are exposed. A non-empty list without `mcp__` entries also keeps MCP tools
    /// registered for codemode and tool_search; only tool_search can declare them. An empty list
    /// exposes no tools.
    pub allowed_tool_names: Option<Vec<String>>,
    /// Optional denylist of tool names or patterns. When provided, matching tools are not exposed.
    pub excluded_tool_names: Option<Vec<String>>,
    /// Override base tools (useful for custom runtimes).
    ///
    /// These are synthesized into minimal ToolDefinitions internally so AgentSession can keep
    /// a definition-first registry even when callers provide plain AgentTool instances.
    pub base_tools_override: Option<IndexMap<String, AgentToolRef>>,
    /// Mutable ref used by Agent to access the current ExtensionRunner
    pub extension_runner_ref: Option<AgentSessionConfigExtensionRunnerRef>,
    /// Session start event metadata emitted when extensions bind to this runtime.
    pub session_start_event: Option<SessionStartEvent>,
}

#[derive(Clone, Default)]
pub struct ExtensionBindings {
    pub ui_context: Option<Arc<dyn ExtensionUIContext>>,
    pub mode: Option<ExtensionMode>,
    pub command_context_actions: Option<Arc<dyn ExtensionCommandContextActions>>,
    pub abort_handler: Option<Arc<dyn Fn() + Send + Sync>>,
    pub shutdown_handler: Option<ShutdownHandler>,
    pub on_error: Option<ExtensionErrorListener>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QueuedInputDisposition {
    #[serde(rename = "handled")]
    Handled,
    #[serde(rename = "queued")]
    Queued,
}

impl QueuedInputDisposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Queued => "queued",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptDisposition {
    #[serde(rename = "handled")]
    Handled,
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "started")]
    Started,
}

impl PromptDisposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Queued => "queued",
            Self::Started => "started",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptOptionsStreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

impl PromptOptionsStreamingBehavior {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// Options for AgentSession.prompt()
#[derive(Clone, Default)]
pub struct PromptOptions {
    /// Whether to dispatch extension commands and expand skill commands and prompt templates (default: true)
    pub expand_prompt_templates: Option<bool>,
    /// Image attachments
    pub images: Option<Vec<ImageContent>>,
    /// When streaming, how to queue the message: "steer" (interrupt) or "followUp" (wait). Required if streaming.
    pub streaming_behavior: Option<PromptOptionsStreamingBehavior>,
    /// Source of input for extension input event handlers. Defaults to "interactive".
    pub source: Option<InputSource>,
    /// Internal hook used by RPC mode to observe how an accepted prompt was dispatched. Not called if the prompt is rejected.
    pub preflight_result: Option<Arc<dyn Fn(PromptDisposition) + Send + Sync>>,
}

/// Options for model/thinking mutations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMutationOptions {
    /// Persist the new value to global defaults. Defaults to session-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
}

/// Result from cycleModel()
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCycleResult {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// Whether cycling through scoped models (--models flag) or all available
    pub is_scoped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatsTokens {
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub input: f64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub output: f64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub cache_read: f64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub cache_write: f64,
    pub total: i64,
}

/// Session statistics for /session command
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: i64,
    pub assistant_messages: i64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub tool_calls: f64,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub tool_results: f64,
    pub total_messages: i64,
    pub tokens: SessionStatsTokens,
    #[serde(with = "pi_ai::serde_util::js_f64")]
    pub cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

#[derive(Clone)]
pub struct ToolDefinitionEntry {
    pub definition: ToolDefinition,
    pub source_info: SourceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRoutedModel {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

/// `"all" | "one-at-a-time"`, kept verbatim when settings hold another value (like TS).
pub type AgentSessionSteeringMode = QueueMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionScopedModels {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSetScopedModelsScopedModels {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSteerOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InputSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionFollowUpOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InputSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSendCustomMessageMessage {
    pub custom_type: String,
    pub content: UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentSessionSendCustomMessageOptionsDeliverAs {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
    #[serde(rename = "nextTurn")]
    NextTurn,
}

impl AgentSessionSendCustomMessageOptionsDeliverAs {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
            Self::NextTurn => "nextTurn",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSendCustomMessageOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<AgentSessionSendCustomMessageOptionsDeliverAs>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSendUserMessageOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<PromptOptionsStreamingBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand_prompt_templates: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionClearQueueResult {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentSessionCycleModelDirection {
    #[serde(rename = "forward")]
    Forward,
    #[serde(rename = "backward")]
    Backward,
}

impl AgentSessionCycleModelDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }
}

#[derive(Clone, Default)]
pub struct AgentSessionReloadOptions {
    pub before_session_start:
        Option<Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct AgentSessionExecuteBashOptions {
    pub exclude_from_context: Option<bool>,
    pub id: Option<String>,
    pub operations: Option<BashOperations>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecordBashResultOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionNavigateTreeOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionNavigateTreeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_text: Option<String>,
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_entry: Option<BranchSummaryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionGetUserMessagesForForkingResult {
    pub entry_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionExportToHtmlOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentSessionSummarizeForBugReportOptions {
    pub hint: Option<String>,
    pub signal: CancellationToken,
}

fn estimate_messages_tokens(messages: &[AgentMessage]) -> u64 {
    messages.iter().map(estimate_tokens).sum()
}

fn without_deleted_headers(
    headers: Option<&pi_ai::types::ProviderHeaders>,
) -> Option<IndexMap<String, String>> {
    headers.map(|headers| {
        headers
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
            .collect()
    })
}

fn to_index_env(env: Option<&pi_ai::types::ProviderEnv>) -> Option<IndexMap<String, String>> {
    env.map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn user_content_text(content: &UserContent) -> String {
    pi_ai::utils::text::content_text(content, "")
}

fn agent_message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(m) => user_content_text(&m.content),
        _ => String::new(),
    }
}

fn custom_message_to_agent(message: &CustomMessage) -> AgentMessage {
    AgentMessage::from(message.clone())
}

/// The retry policy of the session's retry settings.
fn retry_policy(settings: &SettingsManager) -> RetryPolicy {
    let retry = settings.get_retry_settings();
    RetryPolicy {
        enabled: retry.enabled,
        max_retries: retry.max_retries.max(0) as u32,
        base_delay_ms: retry.base_delay_ms.max(0) as u64,
        max_agent_delay_ms: Some(retry.max_agent_delay_ms.max(0) as u64),
    }
}

fn compaction_settings_model(model: Option<&Model>) -> Option<SettingsManagerGetCompactionSettingsModel> {
    model.map(|m| SettingsManagerGetCompactionSettingsModel::new(m.provider.clone(), m.id.clone()))
}

/// Error with an extension-facing message (TS `new Error(message)`).
fn err(message: impl Into<String>) -> anyhow::Error {
    anyhow::anyhow!(message.into())
}

/// Run `future` detached, like TS `void promise`: an async function runs synchronously until its
/// first real suspension, so its effects up to that point (queueing a message, setting abort
/// flags) are visible to the caller right away. The rest continues on a tokio task. Requires a
/// tokio runtime; without one the future is dropped.
fn spawn_detached(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut future = Box::pin(future);
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    if future.as_mut().poll(&mut cx).is_ready() {
        return;
    }
    // The task polls again with a real waker before anything can be missed.
    crate::utils::uncaught::spawn_on(&handle, future);
}

/// Request auth for one provider request.
struct RequestAuth {
    model: Model,
    api_key: Option<String>,
    headers: Option<IndexMap<String, String>>,
    env: Option<IndexMap<String, String>>,
}

struct SummarizationRequestAuth {
    model: Model,
    api_key: Option<String>,
    headers: Option<IndexMap<String, String>>,
    env: Option<IndexMap<String, String>>,
    thinking_level: ThinkingLevel,
}

/// Action deferred until `agent_settled` listeners return.
type DeferredAction =
    Box<dyn FnOnce(Arc<AgentSession>) -> BoxFuture<'static, anyhow::Result<()>> + Send>;

/// Mutable state of an [`AgentSession`]. Guarded by a short-lived lock: never held across an
/// `.await`, a listener call, or an extension call.
struct SessionState {
    scoped_models: Vec<AgentSessionScopedModels>,
    listeners: Vec<(u64, AgentSessionEventListener)>,
    next_listener_id: u64,
    is_agent_run_active: bool,
    agent_run_abort_requested: bool,

    /// Tracks pending steering messages for UI display. Removed when delivered.
    steering_messages: Vec<String>,
    /// Tracks pending follow-up messages for UI display. Removed when delivered.
    follow_up_messages: Vec<String>,
    /// Messages queued to be included with the next user prompt as context ("asides").
    pending_next_turn_messages: Vec<CustomMessage>,
    /// Context-only custom messages queued during a run, flushed once the current turn's tool results are in.
    pending_custom_messages: Vec<CustomMessage>,

    compaction_abort: Option<CancellationToken>,
    auto_compaction_abort: Option<(u64, CancellationToken)>,
    overflow_recovery_attempted: bool,
    branch_summary_abort: Option<CancellationToken>,
    retry_abort: Option<CancellationToken>,
    retry_attempt: u64,
    /// Failed response that the next request repeats, set by auto-retry and overflow recovery.
    failed_response: Option<AssistantMessage>,

    bash_aborts: Vec<(u64, CancellationToken)>,
    pending_bash_messages: Vec<BashExecutionMessage>,

    extension_runner: Option<Arc<ExtensionRunner>>,
    turn_index: u64,
    /// Entry ids of persisted messages (TS `WeakMap` keyed by message identity; Rust compares values).
    entry_ids_by_message: Vec<(AgentMessage, String)>,
    /// Set by a listener of the `message_end` event being emitted (see
    /// [`AgentSession::replace_message_end_message`]).
    message_end_replacement: Option<AgentMessage>,
    /// Messages whose turn_end boundary already ran in `finish_turn`.
    boundary_dispatched_messages: Vec<AgentMessage>,
    last_assistant_message: Option<AssistantMessage>,
    last_assistant_tool_results: Vec<AgentMessage>,
    last_activity_outcome: AgentActivityOutcome,
    is_before_settle: bool,
    abort_during_before_settle: bool,
    is_emitting_agent_settled: bool,
    deferred_settled_actions: Vec<DeferredAction>,

    base_tool_definitions: IndexMap<String, ToolDefinition>,
    /// Tools of the restored or reloaded loadout that are not registered yet.
    pending_tool_names: IndexSet<String>,
    extension_ui_context: Option<Arc<dyn ExtensionUIContext>>,
    extension_mode: ExtensionMode,
    extension_command_context_actions: Option<Arc<dyn ExtensionCommandContextActions>>,
    extension_abort_handler: Option<Arc<dyn Fn() + Send + Sync>>,
    extension_shutdown_handler: Option<ShutdownHandler>,
    extension_error_listener: Option<ExtensionErrorListener>,
    extension_error_unsubscriber: Option<Arc<dyn Fn() + Send + Sync>>,

    tool_registry: IndexMap<String, AgentToolRef>,
    nested_tool_calls: Option<Arc<NestedToolCallRunner>>,
    hidden_declarations: HashSet<String>,
    tool_definitions: IndexMap<String, ToolDefinitionEntry>,
    tool_prompt_snippets: IndexMap<String, String>,
    tool_prompt_guidelines: IndexMap<String, Vec<String>>,

    base_system_prompt_options: NormalizedBuildSystemPromptOptions,
    /// Prompt options after before_agent_start mutations for the active run.
    run_system_prompt_options: Option<NormalizedBuildSystemPromptOptions>,
    next_id: u64,
}

pub struct AgentSession {
    weak: Weak<AgentSession>,
    agent: Agent,
    session_manager: Arc<SessionManager>,
    settings_manager: Arc<SettingsManager>,
    model_runtime: Arc<ModelRuntime>,
    cache_warmer: Option<Arc<CacheWarmer>>,
    resource_loader: Arc<dyn ResourceLoader>,
    custom_tools: Vec<ToolDefinition>,
    cwd: String,
    extension_runner_ref: Option<AgentSessionConfigExtensionRunnerRef>,
    initial_active_tool_names: Option<Vec<String>>,
    uses_default_tools: bool,
    default_tool_modifiers: Vec<String>,
    /// Matches the `--tools` entries: tool names or patterns.
    allowed_tools: Option<Arc<dyn Fn(String) -> bool + Send + Sync>>,
    /// Whether the allowlist filters MCP tools: it is empty (`--no-tools`) or names an MCP tool.
    allowlist_filters_mcp: bool,
    excluded_tools: Option<Arc<dyn Fn(String) -> bool + Send + Sync>>,
    base_tools_override: Option<IndexMap<String, AgentToolRef>>,
    session_start_event: SessionStartEvent,
    agent_subscription: Mutex<Option<pi_agent::Subscription>>,
    state: Mutex<SessionState>,
    /// Bumped whenever the session may have become idle (TS idle-wait promise).
    idle_tx: tokio::sync::watch::Sender<u64>,
    /// Test-only replacement of `run_auto_compaction` (TS tests spy on the private method).
    run_auto_compaction_override: Mutex<Option<RunAutoCompactionOverride>>,
}

/// See [`AgentSession::set_run_auto_compaction_override`].
pub type RunAutoCompactionOverride = Arc<dyn Fn(CompactionReason, bool) -> bool + Send + Sync>;

impl AgentSession {
    pub fn new(config: AgentSessionConfig) -> Arc<Self> {
        let (allowed_tools, allowlist_filters_mcp) = match &config.allowed_tool_names {
            Some(names) => (
                Some(create_tool_name_matcher(names)),
                names.is_empty() || names.iter().any(|entry| entry.starts_with("mcp__")),
            ),
            None => (None, false),
        };
        let excluded_tools = config
            .excluded_tool_names
            .as_ref()
            .map(|names| create_tool_name_matcher(names));
        let (idle_tx, _) = tokio::sync::watch::channel(0u64);
        let session = Arc::new_cyclic(|weak: &Weak<AgentSession>| AgentSession {
            weak: weak.clone(),
            agent: config.agent.clone(),
            session_manager: config.session_manager.clone(),
            settings_manager: config.settings_manager.clone(),
            model_runtime: config.model_runtime.clone(),
            cache_warmer: config.cache_warmer.clone(),
            resource_loader: config.resource_loader.clone(),
            custom_tools: config.custom_tools.clone().unwrap_or_default(),
            cwd: config.cwd.clone(),
            extension_runner_ref: config.extension_runner_ref.clone(),
            initial_active_tool_names: config.initial_active_tool_names.clone(),
            uses_default_tools: config.uses_default_tools.unwrap_or(false),
            default_tool_modifiers: config.default_tool_modifiers.clone().unwrap_or_default(),
            allowed_tools,
            allowlist_filters_mcp,
            excluded_tools,
            base_tools_override: config.base_tools_override.clone(),
            session_start_event: config.session_start_event.clone().unwrap_or(SessionStartEvent {
                reason: SessionStartReason::Startup,
                previous_session_file: None,
            }),
            agent_subscription: Mutex::new(None),
            run_auto_compaction_override: Mutex::new(None),
            state: Mutex::new(SessionState {
                scoped_models: config
                    .scoped_models
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| AgentSessionScopedModels {
                        model: m.model,
                        thinking_level: m.thinking_level,
                    })
                    .collect(),
                listeners: Vec::new(),
                next_listener_id: 1,
                is_agent_run_active: false,
                agent_run_abort_requested: false,
                steering_messages: Vec::new(),
                follow_up_messages: Vec::new(),
                pending_next_turn_messages: Vec::new(),
                pending_custom_messages: Vec::new(),
                compaction_abort: None,
                auto_compaction_abort: None,
                overflow_recovery_attempted: false,
                branch_summary_abort: None,
                retry_abort: None,
                retry_attempt: 0,
                failed_response: None,
                bash_aborts: Vec::new(),
                pending_bash_messages: Vec::new(),
                extension_runner: None,
                turn_index: 0,
                entry_ids_by_message: Vec::new(),
                message_end_replacement: None,
                boundary_dispatched_messages: Vec::new(),
                last_assistant_message: None,
                last_assistant_tool_results: Vec::new(),
                last_activity_outcome: AgentActivityOutcome::Completed,
                is_before_settle: false,
                abort_during_before_settle: false,
                is_emitting_agent_settled: false,
                deferred_settled_actions: Vec::new(),
                base_tool_definitions: IndexMap::new(),
                pending_tool_names: IndexSet::new(),
                extension_ui_context: None,
                extension_mode: ExtensionMode::Print,
                extension_command_context_actions: None,
                extension_abort_handler: None,
                extension_shutdown_handler: None,
                extension_error_listener: None,
                extension_error_unsubscriber: None,
                tool_registry: IndexMap::new(),
                nested_tool_calls: None,
                hidden_declarations: HashSet::new(),
                tool_definitions: IndexMap::new(),
                tool_prompt_snippets: IndexMap::new(),
                tool_prompt_guidelines: IndexMap::new(),
                base_system_prompt_options: normalize_build_system_prompt_options(
                    &BuildSystemPromptOptions::new(config.cwd.clone()),
                ),
                run_system_prompt_options: None,
                next_id: 1,
            }),
            idle_tx,
        });

        if let Some(warmer) = &session.cache_warmer {
            let weak = session.weak.clone();
            warmer.set_on_warmed(Some(Arc::new(move |entry: UsageEntry| {
                if let Some(session) = weak.upgrade() {
                    session.emit(AgentSessionEvent::EntryAppended {
                        entry: SessionEntry::Usage(entry),
                    });
                }
            })));
        }

        // Always subscribe to agent events for internal handling
        // (session persistence, extensions, auto-compaction, retry logic)
        let weak = session.weak.clone();
        let subscription = session.agent.subscribe(Arc::new(move |event, _signal| {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(session) = weak.upgrade() {
                    session.handle_agent_event(event).await;
                }
            })
        }));
        *session.agent_subscription.lock() = Some(subscription);
        session.install_agent_tool_hooks();
        session.install_agent_next_turn_refresh();
        session.install_agent_request_projection();
        session.install_agent_boundary_hooks();
        session.install_hidden_declarations_projection();
        session.install_agent_forced_prompt_projection();

        session.build_runtime(session.initial_active_tool_names.clone(), None, true);
        if session.uses_default_tools {
            let modifiers = session.default_tool_modifiers.clone();
            session.restore_tools_from_transcript(&modifiers);
        }
        session
    }

    fn arc(&self) -> Arc<AgentSession> {
        self.weak.upgrade().expect("AgentSession used after drop")
    }

    fn runner(&self) -> Arc<ExtensionRunner> {
        self.state
            .lock()
            .extension_runner
            .clone()
            .expect("extension runner is created in the constructor")
    }

    pub fn agent(&self) -> Agent {
        self.agent.clone()
    }

    pub fn session_manager(&self) -> Arc<SessionManager> {
        self.session_manager.clone()
    }

    pub fn settings_manager(&self) -> Arc<SettingsManager> {
        self.settings_manager.clone()
    }

    pub fn model_runtime(&self) -> Arc<ModelRuntime> {
        self.model_runtime.clone()
    }

    async fn get_required_request_auth(
        &self,
        model: &Model,
        signal: Option<CancellationToken>,
    ) -> anyhow::Result<RequestAuth> {
        let result = self
            .model_runtime
            .get_auth(
                &ModelRuntimeGetAuthProviderOrModel::AnyModel(AnyModel::Chat(model.clone())),
                Some(&ModelRuntimeAuthOverrides {
                    signal,
                    api_key: None,
                    env: None,
                    min_o_auth_validity_ms: None,
                }),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if error
                    .chain()
                    .skip(1)
                    .any(|cause| cause.to_string() == "authHeader requires a resolved API key")
                {
                    return Err(err(format_no_api_key_found_message(&model.provider)));
                }
                return Err(error);
            }
        };
        if let Some(result) = result
            && (result.auth.api_key.is_some() || result.auth.headers.is_some())
        {
            let mut request_model = model.clone();
            if let Some(base_url) = &result.auth.base_url {
                request_model.base_url = base_url.clone();
            }
            return Ok(RequestAuth {
                model: request_model,
                api_key: result.auth.api_key.clone(),
                headers: without_deleted_headers(result.auth.headers.as_ref()),
                env: to_index_env(result.env.as_ref()),
            });
        }
        if self.model_runtime.is_using_o_auth(&model.provider) {
            return Err(err(format!(
                "Authentication failed for \"{0}\". Credentials may have expired or network is unavailable. Run '/login {0}' to re-authenticate.",
                model.provider
            )));
        }
        Err(err(format_no_api_key_found_message(&model.provider)))
    }

    async fn get_summarization_request_auth(
        &self,
        selected_model: &Model,
        signal: Option<CancellationToken>,
    ) -> anyhow::Result<SummarizationRequestAuth> {
        // Route a virtual model first: summaries size their input and output from the model they get.
        let (model, thinking_level) = if is_virtual_model(selected_model) {
            let route = self
                .model_runtime
                .resolve_model(
                    selected_model.clone(),
                    &convert_to_llm(&self.messages()),
                    &ModelRuntimeResolveModelOptions {
                        reason: ModelRouteReason::Direct,
                        thinking_level: self.thinking_level(),
                        signal: signal.clone(),
                        failed: None,
                        state: None,
                    },
                )
                .await?;
            (route.model, route.thinking_level)
        } else {
            (selected_model.clone(), self.thinking_level())
        };
        // The strict variant applies when the agent streams through pi-ai's global
        // `streamSimple`; a custom stream function (the SDK installs one) resolves auth itself, so
        // auth problems fall back to an unauthenticated request here.
        if self
            .agent
            .stream_fn()
            .is_some_and(|stream_fn| pi_agent::is_compat_stream_simple_fn(&stream_fn))
        {
            let auth = self.get_required_request_auth(&model, signal).await?;
            return Ok(SummarizationRequestAuth {
                model: auth.model,
                api_key: auth.api_key,
                headers: auth.headers,
                env: auth.env,
                thinking_level,
            });
        }
        match self
            .model_runtime
            .get_auth(
                &ModelRuntimeGetAuthProviderOrModel::AnyModel(AnyModel::Chat(model.clone())),
                Some(&ModelRuntimeAuthOverrides {
                    signal: signal.clone(),
                    api_key: None,
                    env: None,
                    min_o_auth_validity_ms: None,
                }),
            )
            .await
        {
            Ok(None) => Ok(SummarizationRequestAuth {
                model,
                api_key: None,
                headers: None,
                env: None,
                thinking_level,
            }),
            Ok(Some(result)) => {
                let mut request_model = model.clone();
                if let Some(base_url) = &result.auth.base_url {
                    request_model.base_url = base_url.clone();
                }
                Ok(SummarizationRequestAuth {
                    model: request_model,
                    api_key: result.auth.api_key.clone(),
                    headers: without_deleted_headers(result.auth.headers.as_ref()),
                    env: to_index_env(result.env.as_ref()),
                    thinking_level,
                })
            }
            Err(error) => {
                if signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                    return Err(error);
                }
                Ok(SummarizationRequestAuth {
                    model,
                    api_key: None,
                    headers: None,
                    env: None,
                    thinking_level,
                })
            }
        }
    }

    /// The model whose limits apply to `message`, or `None` when the message came from another
    /// model. Under a virtual selection, that is the physical model that produced it.
    fn model_for_message(&self, message: &AssistantMessage) -> Option<Model> {
        let model = self.model()?;
        if is_virtual_model(&model) {
            return self
                .model_runtime
                .get_physical_model(&message.provider, &message.model);
        }
        (model.provider == message.provider && model.id == message.model).then_some(model)
    }

    /// Record the selection on the current branch when the branch implies another one, so a resume
    /// restores it.
    fn record_selection(&self) {
        let Some(model) = self.model() else {
            return;
        };
        let runtime = self.model_runtime.clone();
        let get_model: Arc<dyn Fn(String, String) -> Option<Model> + Send + Sync> =
            Arc::new(move |provider, model_id| runtime.get_model(&provider, &model_id));
        let recorded = get_branch_selection(&self.session_manager.get_branch(None), get_model.clone());
        let Some(recorded) = recorded else {
            return;
        };
        if recorded.provider == model.provider && recorded.model_id == model.id {
            return;
        }
        let recorded_model = get_model(recorded.provider.clone(), recorded.model_id.clone());
        if !is_virtual_model(&model) && !recorded_model.is_some_and(|m| is_virtual_model(&m)) {
            return;
        }
        let _ = self
            .session_manager
            .append_model_change(&model.provider, &model.id);
    }

    /// The model whose limits apply to the conversation.
    fn limits_model(&self) -> Option<Model> {
        self.routed_model().map(|r| r.model).or_else(|| self.model())
    }

    // =========================================================================
    // Agent hooks
    // =========================================================================

    /// Install tool hooks once on the Agent instance. The callbacks read the current extension
    /// runner at execution time, so extension reload swaps in the new runner without reinstalling.
    fn install_agent_tool_hooks(&self) {
        let weak = self.weak.clone();
        self.agent
            .set_before_tool_call(Some(Arc::new(move |context, _signal| {
                let weak = weak.clone();
                Box::pin(async move {
                    match weak.upgrade() {
                        Some(session) => session.before_tool_call(context, None).await,
                        None => Ok(None),
                    }
                })
            })));
        let weak = self.weak.clone();
        self.agent
            .set_after_tool_call(Some(Arc::new(move |context, _signal| {
                let weak = weak.clone();
                Box::pin(async move {
                    match weak.upgrade() {
                        Some(session) => session.after_tool_call(context, None).await,
                        None => Ok(None),
                    }
                })
            })));
    }

    /// `tool_call` handlers. `parent_tool_call_id` is set for calls another tool made.
    async fn before_tool_call(
        &self,
        context: BeforeToolCallContext,
        parent_tool_call_id: Option<String>,
    ) -> anyhow::Result<Option<BeforeToolCallResult>> {
        let runner = self.runner();
        if !runner.has_handlers("tool_call") {
            return Ok(None);
        }
        let input = match &context.args {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        let result = runner
            .emit_tool_call(&ToolCallEvent {
                tool_call_id: context.tool_call.id.clone(),
                parent_tool_call_id,
                tool_name: context.tool_call.name.clone(),
                input,
            })
            .await?;
        Ok(result.map(|r| BeforeToolCallResult {
            block: r.block == Some(true),
            reason: r.reason,
            terminate: r.terminate,
            args: r.input.map(Value::Object),
        }))
    }

    /// `tool_result` handlers and image normalization. `parent_tool_call_id` is set for calls
    /// another tool made.
    async fn after_tool_call(
        &self,
        context: AfterToolCallContext,
        parent_tool_call_id: Option<String>,
    ) -> anyhow::Result<Option<AfterToolCallResult>> {
        let runner = self.runner();
        let result = &context.result;
        let hook_result = if runner.has_handlers("tool_result") {
            let input = match &context.args {
                Value::Object(map) => map.clone(),
                _ => serde_json::Map::new(),
            };
            runner
                .emit_tool_result(&ToolResultEvent {
                    tool_call_id: context.tool_call.id.clone(),
                    parent_tool_call_id,
                    tool_name: context.tool_call.name.clone(),
                    input,
                    content: result.content.clone(),
                    structured_content: result.structured_content.clone(),
                    is_error: context.is_error,
                    usage: result.usage.clone(),
                    details: result.details.clone(),
                })
                .await?
        } else {
            None
        };

        let content = hook_result
            .as_ref()
            .and_then(|h| h.content.clone())
            .unwrap_or_else(|| result.content.clone());
        // Runs after the extension hook so images injected or replaced by extensions are normalized too.
        let has_images = content
            .iter()
            .any(|block| matches!(block, UserContentBlock::Image(_)));
        let normalized = if has_images {
            let resize_options = self
                .limits_model()
                .and_then(|m| m.input_limits)
                .and_then(|l| l.images)
                .and_then(|i| i.resize);
            Some(
                normalize_tool_result_images(
                    &content,
                    Some(&NormalizeToolResultImagesOptions {
                        auto_resize_images: Some(self.settings_manager.get_image_auto_resize()),
                        resize_options,
                    }),
                )
                .await?,
            )
        } else {
            None
        };
        let changed = normalized.as_ref().is_some_and(|n| *n != content);
        if hook_result.is_none() && !changed {
            return Ok(None);
        }
        let normalized_content = normalized.unwrap_or(content);
        // The hook result already dropped structured content that replaced content no longer matches.
        Ok(Some(AfterToolCallResult {
            content: Some(normalized_content),
            details: hook_result.as_ref().and_then(|h| h.details.clone()),
            structured_content: match &hook_result {
                Some(h) => h.structured_content.clone(),
                None => result.structured_content.clone(),
            },
            is_error: Some(
                hook_result
                    .as_ref()
                    .and_then(|h| h.is_error)
                    .unwrap_or(context.is_error),
            ),
            usage: hook_result.as_ref().and_then(|h| h.usage.clone()),
            terminate: None,
        }))
    }

    fn nested_tool_runner(&self) -> Arc<NestedToolCallRunner> {
        let mut state = self.state.lock();
        if let Some(runner) = &state.nested_tool_calls {
            return runner.clone();
        }
        let runner = Arc::new(NestedToolCallRunner::new(Arc::new(SessionNestedHost {
            session: self.weak.clone(),
        })));
        state.nested_tool_calls = Some(runner.clone());
        runner
    }

    /// Run a call that the tool call `parent_tool_call_id` made through `ctx.executeTool()`. It
    /// goes through the agent's tool pipeline with the session's hooks, against the callable tools.
    async fn execute_nested_tool_call(
        &self,
        parent_tool_call_id: String,
        name: String,
        args: Value,
        options: ExecuteToolOptions,
    ) -> AgentToolCallOutcome {
        let runner = self.nested_tool_runner();
        runner
            .execute(
                &parent_tool_call_id,
                &name,
                args,
                Some(&NestedToolCallOptions {
                    signal: options.signal,
                    on_update: options.on_update,
                }),
            )
            .await
    }

    /// Whether `projection`, the current session projection, exceeds the compaction threshold of `model`.
    fn exceeds_compaction_threshold(&self, model: &Model, projection: &SessionProjection) -> bool {
        if model.context_window == 0 {
            return false;
        }
        let Ok(settings) = self.compaction_settings(self.model().as_ref()) else {
            return false;
        };
        should_compact(
            estimate_projected_context_tokens(projection, &self.session_manager.get_branch(None))
                .tokens,
            model.context_window,
            &settings,
        )
    }

    fn compaction_settings(&self, model: Option<&Model>) -> anyhow::Result<CompactionSettings> {
        let settings = self
            .settings_manager
            .get_compaction_settings(compaction_settings_model(model).as_ref())?;
        Ok(CompactionSettings {
            enabled: settings.enabled,
            reserve_tokens: settings.reserve_tokens,
            keep_recent_tokens: settings.keep_recent_tokens,
        })
    }

    async fn compact_before_next_assistant_response(&self, context: AgentContext) -> AgentContext {
        let projection = self.session_manager.build_session_projection();
        // A virtual selection is checked in prepareRequest, against the model the request is routed to.
        let model = self.model();
        let needs = match &model {
            Some(model) => {
                !is_virtual_model(model) && self.exceeds_compaction_threshold(model, &projection)
            }
            None => false,
        };
        if !needs {
            return AgentContext {
                messages: projection.messages,
                tools: context.tools,
            };
        }
        self.run_auto_compaction(CompactionReason::Threshold, false)
            .await;
        AgentContext {
            messages: self.session_manager.build_session_projection().messages,
            tools: context.tools,
        }
    }

    fn install_agent_request_projection(&self) {
        let previous = self.agent.prepare_request();
        let weak = self.weak.clone();
        self.agent.set_prepare_request(Some(Arc::new(move |request, signal| {
            let weak = weak.clone();
            let previous = previous.clone();
            Box::pin(async move {
                let Some(session) = weak.upgrade() else {
                    return Ok(None);
                };
                session.prepare_request(request, signal, previous).await
            })
        })));
    }

    async fn prepare_request_inner(
        &self,
        request: &PrepareRequestContext,
        signal: &CancellationToken,
        previous: &Option<PrepareRequestFn>,
    ) -> anyhow::Result<(Option<AgentRequestUpdate>, AgentContext, SessionProjection)> {
        let projection = self.session_manager.build_session_projection();
        let canonical_context = AgentContext {
            messages: projection.messages.clone(),
            // Messages declare the provider-visible loadout; context.tools keeps executable implementations.
            tools: Some(self.agent.tools()),
        };
        let previous_update = match previous {
            Some(previous) => {
                previous(
                    PrepareRequestContext {
                        context: canonical_context.clone(),
                        model: self.agent.model(),
                        thinking_level: self.agent.thinking_level(),
                    },
                    signal.clone(),
                )
                .await?
            }
            None => None,
        };
        let context = previous_update
            .as_ref()
            .and_then(|p| p.context.clone())
            .unwrap_or(canonical_context);
        let _ = request;
        Ok((previous_update, context, projection))
    }

    async fn prepare_request(
        &self,
        request: PrepareRequestContext,
        signal: CancellationToken,
        previous: Option<PrepareRequestFn>,
    ) -> anyhow::Result<Option<AgentRequestUpdate>> {
        let failed = self.state.lock().failed_response.take();
        let (mut previous_update, mut context, projection) =
            self.prepare_request_inner(&request, &signal, &previous).await?;
        let model = previous_update
            .as_ref()
            .and_then(|p| p.model.clone())
            .unwrap_or_else(|| self.agent.model());
        let thinking_level = previous_update
            .as_ref()
            .and_then(|p| p.thinking_level)
            .unwrap_or_else(|| self.agent.thinking_level());
        if !is_virtual_model(&model) {
            return Ok(Some(AgentRequestUpdate {
                context: Some(context),
                model: Some(model),
                thinking_level: Some(thinking_level),
            }));
        }

        // The selection stays in agent state; only this request uses the routed model. Only
        // messages the user wrote start a turn; extension messages can follow them.
        let last_response = context
            .messages
            .iter()
            .rposition(|m| matches!(m, AgentMessage::Assistant(_)));
        let start = last_response.map(|i| i + 1).unwrap_or(0);
        let user_turn = context.messages[start..]
            .iter()
            .any(|m| matches!(m, AgentMessage::User(_)));
        let state = get_virtual_model_state(
            &self.session_manager.get_branch(None),
            &model.provider,
            &model.id,
        );
        let reason = if failed.is_some() {
            ModelRouteReason::Retry
        } else if user_turn {
            ModelRouteReason::User
        } else {
            ModelRouteReason::Continuation
        };
        let route = self
            .model_runtime
            .resolve_model(
                model.clone(),
                &convert_to_llm(&context.messages),
                &ModelRuntimeResolveModelOptions {
                    reason,
                    thinking_level,
                    signal: Some(signal.clone()),
                    failed,
                    state: state.clone(),
                },
            )
            .await;
        // A routing failure rejects, which ends the run with an error response.
        let route = route?;
        if let Some(route_state) = &route.state
            && Some(route_state) != state.as_ref()
        {
            let data = VirtualModelStateData {
                provider: model.provider.clone(),
                model_id: model.id.clone(),
                state: route_state.clone(),
            };
            if let Ok(id) = self.session_manager.append_custom_entry(
                VIRTUAL_MODEL_STATE_ENTRY,
                serde_json::to_value(&data).ok(),
            ) && let Some(entry) = self.session_manager.get_entry(&id)
            {
                self.emit(AgentSessionEvent::EntryAppended { entry });
            }
        }
        // The route stands: the router already decided this request.
        if self.exceeds_compaction_threshold(&route.model, &projection) {
            self.run_auto_compaction(CompactionReason::Threshold, false)
                .await;
            let (p, c, _) = self.prepare_request_inner(&request, &signal, &previous).await?;
            previous_update = p;
            context = c;
        }
        let _ = previous_update;
        Ok(Some(AgentRequestUpdate {
            context: Some(context),
            model: Some(route.model),
            thinking_level: Some(route.thinking_level),
        }))
    }

    async fn dispatch_turn_end_boundary(
        &self,
        message: &AssistantMessage,
        tool_results: &[ToolResultMessage],
    ) -> bool {
        let outcome = match message.stop_reason {
            StopReason::Aborted => AgentActivityOutcome::Aborted,
            StopReason::Error => AgentActivityOutcome::Error,
            _ => AgentActivityOutcome::Completed,
        };
        self.state.lock().last_activity_outcome = outcome;
        let agent_message = AgentMessage::Assistant(message.clone());
        let message_entry_id = self.find_persisted_message_entry_id(&agent_message);
        let runner = self.runner();
        if !runner.has_handlers("turn_end") {
            return false;
        }
        let Some(message_entry_id) = message_entry_id else {
            runner.emit_error(&ExtensionError {
                extension_path: "<boundary>".into(),
                event: "turn_end".into(),
                error: "turn_end could not resolve the persisted assistant entry ID".into(),
                stack: None,
            });
            return false;
        };
        let tool_result_entry_ids: Vec<String> = tool_results
            .iter()
            .filter_map(|r| {
                self.find_persisted_message_entry_id(&AgentMessage::ToolResult(r.clone()))
            })
            .collect();
        let turn_index = self.state.lock().turn_index;
        let boundary = runner
            .emit_boundary(
                &BoundaryBaseEvent::TurnEnd {
                    turn_index,
                    message: agent_message,
                    tool_results: tool_results.to_vec(),
                    message_entry_id,
                    tool_result_entry_ids,
                    outcome,
                },
                self.boundary_context_builder(BoundaryKind::TurnEnd),
            )
            .await;
        let Ok(boundary) = boundary else {
            return false;
        };
        self.commit_boundary_drafts(&boundary.entries);
        if boundary.r#continue {
            let can_continue = self
                .build_boundary_context(&[], BoundaryKind::TurnEnd)
                .map(|c| c.can_continue)
                .unwrap_or(false);
            if !can_continue {
                self.report_invalid_boundary_continuation("turn_end");
                return false;
            }
        }
        boundary.r#continue
    }

    fn install_agent_boundary_hooks(&self) {
        let previous = self.agent.finish_turn();
        let weak = self.weak.clone();
        self.agent.set_finish_turn(Some(Arc::new(move |turn, signal| {
            let weak = weak.clone();
            let previous = previous.clone();
            Box::pin(async move {
                let session = weak.upgrade()?;
                session
                    .state
                    .lock()
                    .boundary_dispatched_messages
                    .push(AgentMessage::Assistant(turn.message.clone()));
                let extension_continue = session
                    .dispatch_turn_end_boundary(&turn.message, &turn.tool_results)
                    .await;
                let previous_decision = match &previous {
                    Some(previous) => previous(turn, signal).await,
                    None => None,
                };
                if previous_decision == Some(AgentTurnDecision::End) {
                    return previous_decision;
                }
                if extension_continue || previous_decision == Some(AgentTurnDecision::Continue) {
                    return Some(AgentTurnDecision::Continue);
                }
                None
            })
        })));
    }

    fn install_agent_next_turn_refresh(&self) {
        let previous_with_context = self.agent.prepare_next_turn_with_context();
        let previous_legacy = self.agent.prepare_next_turn();
        let previous: Option<AgentPrepareNextTurnWithContextFn> = match previous_with_context {
            Some(previous) => Some(previous),
            None => previous_legacy.map(|legacy| {
                let f: AgentPrepareNextTurnWithContextFn =
                    Arc::new(move |_turn, signal| legacy(signal));
                f
            }),
        };
        let weak = self.weak.clone();
        self.agent
            .set_prepare_next_turn_with_context(Some(Arc::new(move |turn, signal| {
                let weak = weak.clone();
                let previous = previous.clone();
                Box::pin(async move {
                    let session = weak.upgrade()?;
                    session.prepare_next_turn(turn, signal, previous).await
                })
            })));
    }

    async fn prepare_next_turn(
        &self,
        turn: PrepareNextTurnContext,
        signal: CancellationToken,
        previous: Option<AgentPrepareNextTurnWithContextFn>,
    ) -> Option<AgentLoopTurnUpdate> {
        let context = self
            .compact_before_next_assistant_response(turn.context.clone())
            .await;
        let previous_snapshot = match &previous {
            Some(previous) => {
                previous(
                    PrepareNextTurnContext {
                        context: context.clone(),
                        ..turn
                    },
                    signal,
                )
                .await
            }
            None => None,
        };
        let next_context = previous_snapshot
            .as_ref()
            .and_then(|p| p.context.clone())
            .unwrap_or(context);
        let (run_options, base_options) = {
            let state = self.state.lock();
            (
                state
                    .run_system_prompt_options
                    .clone()
                    .unwrap_or_else(|| state.base_system_prompt_options.clone()),
                state.base_system_prompt_options.clone(),
            )
        };
        let mut options = run_options.clone();
        options.selected_tools = self.get_active_tool_names();
        let mut snippets = base_options.tool_snippets.clone();
        snippets.extend(run_options.tool_snippets.clone());
        options.tool_snippets = snippets;
        let mut guidelines = base_options.tool_guidelines.clone();
        guidelines.extend(run_options.tool_guidelines.clone());
        options.tool_guidelines = guidelines;
        let update_message = self
            .prepare_prompt_and_tool_loadout(&mut options, Some(&next_context.messages))
            .ok()
            .flatten();
        // Keep session.systemPrompt and ctx.getSystemPrompt() in step with what the provider sees.
        self.state.lock().run_system_prompt_options = Some(options);

        let previous_messages = previous_snapshot.as_ref().and_then(|p| p.messages.clone());
        let messages = match update_message {
            Some(update) => {
                let mut messages = previous_messages.unwrap_or_default();
                messages.push(AgentMessage::System(update));
                Some(messages)
            }
            None => previous_messages,
        };
        Some(AgentLoopTurnUpdate {
            context: Some(AgentContext {
                messages: next_context.messages,
                tools: Some(self.agent.tools()),
            }),
            messages,
            model: Some(self.agent.model()),
            thinking_level: Some(self.agent.thinking_level()),
        })
    }

    // =========================================================================
    // Event Subscription
    // =========================================================================

    fn refresh_finalized_context(&self) {
        let projection = self.session_manager.build_session_projection();
        // Rebuild the message -> entry map from the projection (TS keeps a WeakMap keyed by the
        // projected message objects; Rust keys by value).
        let mapping: Vec<(AgentMessage, String)> = projection
            .entries
            .iter()
            .flat_map(|entry| {
                let id = entry.source_entry.id().to_string();
                entry.messages.iter().map(move |m| (m.clone(), id.clone()))
            })
            .collect();
        self.state.lock().entry_ids_by_message = mapping;
        self.agent.set_messages(projection.messages);
    }

    fn apply_boundary_drafts(
        manager: &SessionManager,
        drafts: &[SessionBoundaryDraft],
    ) -> anyhow::Result<Vec<SessionEntry>> {
        let mut appended = Vec::new();
        for draft in drafts {
            let entry_id = match draft {
                SessionBoundaryDraft::Custom(d) => {
                    manager.append_custom_entry(&d.custom_type, d.data.clone())?
                }
                SessionBoundaryDraft::CustomMessage(d) => manager.append_custom_message_entry(
                    &d.custom_type,
                    d.content.clone(),
                    d.display,
                    d.details.clone(),
                )?,
                SessionBoundaryDraft::ContextEdit(d) => {
                    manager.append_context_edit(&d.target_id, d.replacement.as_ref())?
                }
                SessionBoundaryDraft::Compaction(d) => {
                    let tokens_before = estimate_projected_context_tokens(
                        &manager.build_session_projection(),
                        &manager.get_branch(None),
                    )
                    .tokens;
                    manager.append_compaction(
                        &d.summary,
                        d.first_kept_entry_id.as_deref(),
                        tokens_before,
                        d.details.clone(),
                        Some(true),
                        d.usage.clone(),
                    )?
                }
            };
            if let Some(entry) = manager.get_entry(&entry_id) {
                appended.push(entry);
            }
        }
        Ok(appended)
    }

    fn create_boundary_preview_manager(
        &self,
        drafts: &[SessionBoundaryDraft],
    ) -> anyhow::Result<Arc<SessionManager>> {
        let header = self
            .session_manager
            .get_header()
            .ok_or_else(|| err("Session header is missing"))?;
        let mut entries = vec![FileEntry::Session(header)];
        entries.extend(
            self.session_manager
                .get_branch(None)
                .into_iter()
                .map(FileEntry::Entry),
        );
        let manager = SessionManager::in_memory(Some(&self.cwd), None, Some(entries))?;
        Self::apply_boundary_drafts(&manager, drafts)?;
        Ok(manager)
    }

    fn get_pending_boundary_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.agent.peek_queued_messages();
        let pending = self.state.lock().pending_custom_messages.clone();
        messages.extend(pending.iter().map(custom_message_to_agent));
        messages
    }

    fn build_boundary_context(
        &self,
        drafts: &[SessionBoundaryDraft],
        boundary: BoundaryKind,
    ) -> anyhow::Result<BoundaryContextPreview> {
        let projection = self
            .create_boundary_preview_manager(drafts)?
            .build_session_projection();
        let pending_messages = self.get_pending_boundary_messages();
        let llm_messages = convert_to_llm(&projection.messages);
        let final_role = llm_messages.last().map(message_role);
        let has_non_system_context = llm_messages.iter().any(|m| message_role(m) != "system");
        let context_can_continue = has_non_system_context && final_role != Some("assistant");
        let pending_custom_context = !self.state.lock().pending_custom_messages.is_empty();
        let has_queued = self.agent.has_queued_messages();
        let can_continue = context_can_continue
            || pending_custom_context
            || match boundary {
                BoundaryKind::TurnEnd => has_queued,
                BoundaryKind::AgentBeforeSettle => final_role == Some("assistant") && has_queued,
            };
        Ok(BoundaryContextPreview {
            context_entries: projection.entries,
            context_messages: projection.messages,
            llm_messages,
            pending_messages,
            can_continue,
        })
    }

    fn boundary_context_builder(&self, boundary: BoundaryKind) -> BoundaryContextBuilder {
        let weak = self.weak.clone();
        Arc::new(move |drafts: Vec<SessionBoundaryDraft>| {
            let weak = weak.clone();
            Box::pin(async move {
                let session = weak
                    .upgrade()
                    .ok_or_else(|| err("Session is disposed"))?;
                session.build_boundary_context(&drafts, boundary)
            })
        })
    }

    fn commit_boundary_drafts(&self, drafts: &[SessionBoundaryDraft]) {
        let appended = match Self::apply_boundary_drafts(&self.session_manager, drafts) {
            Ok(appended) => appended,
            Err(error) => {
                tracing::warn!("failed to append boundary entries: {error}");
                Vec::new()
            }
        };
        self.refresh_finalized_context();
        for entry in appended {
            self.emit(AgentSessionEvent::EntryAppended { entry });
        }
    }

    fn report_invalid_boundary_continuation(&self, event: &str) {
        self.runner().emit_error(&ExtensionError {
            extension_path: "<boundary>".into(),
            event: event.into(),
            error: format!("{event} requested continuation without runnable model context"),
            stack: None,
        });
    }

    /// Emit an event to all listeners.
    fn emit(&self, event: AgentSessionEvent) {
        let listeners: Vec<AgentSessionEventListener> = self
            .state
            .lock()
            .listeners
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        for listener in listeners {
            listener(event.clone());
        }
    }

    fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let state = self.state.lock();
            (
                state.steering_messages.clone(),
                state.follow_up_messages.clone(),
            )
        };
        self.emit(AgentSessionEvent::QueueUpdate {
            steering,
            follow_up,
        });
    }

    async fn emit_session_compact_failed(&self, event: SessionCompactFailedEvent) {
        let runner = self.runner();
        if runner.has_handlers("session_compact_failed") {
            let _ = runner
                .emit(ExtensionEvent::SessionCompactFailed(event))
                .await;
        }
    }

    fn resolve_idle_wait_if_idle(&self) {
        if self.is_idle() {
            self.idle_tx.send_modify(|v| *v = v.wrapping_add(1));
        }
    }

    async fn emit_agent_settled(&self) -> anyhow::Result<()> {
        if let Some(warmer) = &self.cache_warmer {
            warmer.on_agent_settled();
        }
        let aborted = {
            let mut state = self.state.lock();
            state.is_agent_run_active = false;
            state.is_emitting_agent_settled = true;
            state.agent_run_abort_requested
        };
        let _ = self
            .runner()
            .emit(ExtensionEvent::AgentSettled(AgentSettledEvent { aborted }))
            .await;
        self.emit(AgentSessionEvent::AgentSettled { aborted });
        self.state.lock().is_emitting_agent_settled = false;
        // The run's transient allocations (requests, stream parsing, tool output) are freed by
        // now; return the pages while the session is idle.
        crate::utils::memory::release_free_memory_in_background();

        let deferred: Vec<DeferredAction> =
            std::mem::take(&mut self.state.lock().deferred_settled_actions);
        if !deferred.is_empty() {
            let session = self.arc();
            let mut result = Ok(());
            for action in deferred {
                if let Err(error) = action(session.clone()).await {
                    result = Err(error);
                    break;
                }
            }
            self.resolve_idle_wait_if_idle();
            return result;
        }
        self.resolve_idle_wait_if_idle();
        Ok(())
    }

    /// Internal handler for agent events.
    async fn handle_agent_event(&self, event: AgentEvent) {
        // An abort requested while the run was starting, e.g. during before_agent_start, stops it
        // here. agent_start is awaited before the first request.
        if matches!(event, AgentEvent::AgentStart) && self.state.lock().agent_run_abort_requested {
            self.agent.abort();
        }
        let mut event = event;

        // Record the calls a tool made through ctx.executeTool() and their usage on its result message.
        let nested = self.state.lock().nested_tool_calls.clone();
        if let Some(nested) = nested {
            match &event {
                AgentEvent::MessageStart {
                    message: AgentMessage::ToolResult(message),
                } => {
                    if let Some(summary) = nested.take_record(&message.tool_call_id) {
                        let mut patched = message.clone();
                        if let Some(calls) = summary.calls {
                            patched.nested_calls = Some(calls);
                        }
                        if let Some(usage) = summary.usage {
                            patched.usage = Some(match &patched.usage {
                                Some(existing) => combine_usage(existing, &usage),
                                None => usage,
                            });
                        }
                        // TS mutates the shared message object: the agent loop's copy, agent
                        // state and later events carry the record too.
                        let patched = AgentMessage::ToolResult(patched);
                        self.agent.replace_event_message(patched.clone());
                        event = AgentEvent::MessageStart { message: patched };
                    }
                }
                AgentEvent::AgentEnd { .. } => nested.clear(),
                _ => {}
            }
        }
        // When a user message starts, check if it's from either queue and remove it BEFORE
        // emitting. This ensures the UI sees the updated queue state.
        if let AgentEvent::MessageStart {
            message: AgentMessage::User(user),
        } = &event
        {
            let text = user_content_text(&user.content);
            let changed = {
                let mut state = self.state.lock();
                state.overflow_recovery_attempted = false;
                if text.is_empty() {
                    false
                } else if let Some(i) = state.steering_messages.iter().position(|m| *m == text) {
                    state.steering_messages.remove(i);
                    true
                } else if let Some(i) = state.follow_up_messages.iter().position(|m| *m == text) {
                    state.follow_up_messages.remove(i);
                    true
                } else {
                    false
                }
            };
            if changed {
                self.emit_queue_update();
            }
        }

        // Emit to extensions first, then notify public listeners.
        let event = self.emit_extension_event(event).await;
        let public_event = match &event {
            AgentEvent::AgentEnd { messages } => Some(AgentSessionEvent::AgentEnd {
                messages: messages.clone(),
                will_retry: self.will_retry_after_agent_end(messages),
            }),
            other => AgentSessionEvent::from_agent_event(other.clone(), None),
        };
        let is_message_end = matches!(event, AgentEvent::MessageEnd { .. });
        if is_message_end {
            self.state.lock().message_end_replacement = None;
        }
        if let Some(public_event) = public_event {
            self.emit(public_event);
        }
        // TS listeners share the message object and may mutate it (interactive mode writes the
        // abort text into `errorMessage`); persistence, agent state and later events see that.
        let replacement = if is_message_end { self.state.lock().message_end_replacement.take() } else { None };
        let event = match (event, replacement) {
            (AgentEvent::MessageEnd { .. }, Some(message)) => {
                self.agent.replace_event_message(message.clone());
                AgentEvent::MessageEnd { message }
            }
            (event, _) => event,
        };

        // Handle session persistence
        if let AgentEvent::MessageEnd { message } = &event {
            let entry_id = match message {
                AgentMessage::Custom(custom) if custom.role == CUSTOM_ROLE => {
                    match custom.to_typed::<CustomMessage>() {
                        Ok(custom) => self
                            .session_manager
                            .append_custom_message_entry(
                                &custom.custom_type,
                                custom.content,
                                custom.display,
                                custom.details,
                            )
                            .ok(),
                        Err(_) => None,
                    }
                }
                AgentMessage::System(_)
                | AgentMessage::User(_)
                | AgentMessage::Assistant(_)
                | AgentMessage::ToolResult(_) => {
                    self.session_manager.append_message(message.clone()).ok()
                }
                // Other message types (bashExecution, compactionSummary, branchSummary) are persisted elsewhere
                _ => None,
            };
            if let Some(entry_id) = entry_id {
                self.state
                    .lock()
                    .entry_ids_by_message
                    .push((message.clone(), entry_id));
            }

            if let AgentMessage::Assistant(assistant) = message {
                let retry_end = {
                    let mut state = self.state.lock();
                    state.last_assistant_message = Some(assistant.clone());
                    if assistant.stop_reason != StopReason::Error
                        && assistant.stop_reason != StopReason::Length
                    {
                        state.overflow_recovery_attempted = false;
                    }
                    // Reset retry counter immediately on successful assistant response.
                    if assistant.stop_reason != StopReason::Error && state.retry_attempt > 0 {
                        let attempt = state.retry_attempt;
                        state.retry_attempt = 0;
                        Some(attempt)
                    } else {
                        None
                    }
                };
                if let Some(attempt) = retry_end {
                    self.emit(AgentSessionEvent::AutoRetryEnd {
                        success: true,
                        attempt,
                        final_error: None,
                    });
                }
            }
        }

        // A turn ends after its assistant message and every tool result has been appended,
        // so this is the first point in the run where a context-only custom message can be
        // inserted without landing between a tool call and its result.
        if let AgentEvent::TurnEnd { tool_results, .. } = &event {
            self.state.lock().last_assistant_tool_results = tool_results
                .iter()
                .cloned()
                .map(AgentMessage::ToolResult)
                .collect();
            self.flush_pending_custom_messages();
        }
    }

    fn will_retry_after_agent_end(&self, messages: &[AgentMessage]) -> bool {
        let (abort_requested, retry_attempt) = {
            let state = self.state.lock();
            (state.agent_run_abort_requested, state.retry_attempt)
        };
        if abort_requested {
            return false;
        }
        let settings = self.settings_manager.get_retry_settings();
        if !settings.enabled || retry_attempt as i64 >= settings.max_retries {
            return false;
        }
        for message in messages.iter().rev() {
            if let AgentMessage::Assistant(assistant) = message {
                return self.is_retryable_error(assistant);
            }
        }
        false
    }

    fn find_persisted_message_entry_id(&self, message: &AgentMessage) -> Option<String> {
        let mapped = self
            .state
            .lock()
            .entry_ids_by_message
            .iter()
            .rev()
            .find(|(m, _)| m == message)
            .map(|(_, id)| id.clone());
        if mapped.is_some() {
            return mapped;
        }
        for entry in self.session_manager.get_branch(None).iter().rev() {
            if let SessionEntry::Message(e) = entry
                && &e.message == message
            {
                return Some(e.id.clone());
            }
        }
        let messages = self.agent.messages();
        let message_index = messages.iter().position(|m| m == message)?;
        let projection = self.session_manager.build_session_projection();
        let mut projected_index = 0usize;
        for entry in &projection.entries {
            for _ in &entry.messages {
                if projected_index == message_index {
                    let id = entry.source_entry.id().to_string();
                    self.state
                        .lock()
                        .entry_ids_by_message
                        .push((message.clone(), id.clone()));
                    return Some(id);
                }
                projected_index += 1;
            }
        }
        None
    }

    fn omit_recovery_attempt(
        &self,
        message: &AssistantMessage,
        tool_results: &[AgentMessage],
    ) -> anyhow::Result<()> {
        let mut targets = vec![AgentMessage::Assistant(message.clone())];
        targets.extend(tool_results.iter().cloned());
        let target_ids: Vec<Option<String>> = targets
            .iter()
            .map(|t| self.find_persisted_message_entry_id(t))
            .collect();
        let state_messages = self.agent.messages();
        let unresolved_projected_target = targets
            .iter()
            .zip(&target_ids)
            .any(|(target, id)| id.is_none() && state_messages.contains(target));
        if unresolved_projected_target {
            anyhow::bail!(
                "Cannot persist recovery omission because a projected message has no source entry"
            );
        }
        for target_id in target_ids.into_iter().flatten() {
            let edit_id = self.session_manager.append_context_edit(&target_id, None)?;
            if let Some(entry) = self.session_manager.get_entry(&edit_id) {
                self.emit(AgentSessionEvent::EntryAppended { entry });
            }
        }
        self.refresh_finalized_context();
        Ok(())
    }

    /// Find the last assistant message in agent state (including aborted ones)
    fn find_last_assistant_message(&self) -> Option<AssistantMessage> {
        self.agent.with_state(|state| {
            state.messages.iter().rev().find_map(|m| match m {
                AgentMessage::Assistant(a) => Some(a.clone()),
                _ => None,
            })
        })
    }

    /// Emit extension events based on agent events. Returns the event, with a `message_end`
    /// message replaced by extension handlers.
    async fn emit_extension_event(&self, event: AgentEvent) -> AgentEvent {
        let runner = self.runner();
        match event {
            AgentEvent::AgentStart => {
                self.state.lock().turn_index = 0;
                let _ = runner
                    .emit(ExtensionEvent::AgentStart(AgentStartEvent {}))
                    .await;
                AgentEvent::AgentStart
            }
            AgentEvent::AgentEnd { messages } => {
                let _ = runner
                    .emit(ExtensionEvent::AgentEnd(AgentEndEvent {
                        messages: messages.clone(),
                    }))
                    .await;
                AgentEvent::AgentEnd { messages }
            }
            AgentEvent::TurnStart => {
                let turn_index = self.state.lock().turn_index;
                let _ = runner
                    .emit(ExtensionEvent::TurnStart(TurnStartEvent {
                        turn_index,
                        timestamp: now_ms(),
                    }))
                    .await;
                AgentEvent::TurnStart
            }
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => {
                if let AgentMessage::Assistant(assistant) = &message {
                    let dispatched = {
                        let mut state = self.state.lock();
                        match state
                            .boundary_dispatched_messages
                            .iter()
                            .position(|m| m == &message)
                        {
                            Some(i) => {
                                state.boundary_dispatched_messages.remove(i);
                                true
                            }
                            None => false,
                        }
                    };
                    if !dispatched {
                        self.dispatch_turn_end_boundary(assistant, &tool_results)
                            .await;
                    }
                }
                self.state.lock().turn_index += 1;
                AgentEvent::TurnEnd {
                    message,
                    tool_results,
                }
            }
            AgentEvent::MessageStart { message } => {
                let _ = runner
                    .emit(ExtensionEvent::MessageStart(MessageStartEvent {
                        message: message.clone(),
                    }))
                    .await;
                AgentEvent::MessageStart { message }
            }
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                let _ = runner
                    .emit(ExtensionEvent::MessageUpdate(MessageUpdateEvent {
                        message: message.clone(),
                        assistant_message_event: (*assistant_message_event).clone(),
                    }))
                    .await;
                AgentEvent::MessageUpdate {
                    message,
                    assistant_message_event,
                }
            }
            AgentEvent::MessageEnd { message } => {
                let replacement = runner
                    .emit_message_end(&MessageEndEvent {
                        message: message.clone(),
                    })
                    .await
                    .ok()
                    .flatten();
                match replacement {
                    Some(replacement) => {
                        // Untyped extension handlers can return messages with null/missing
                        // content; normalize so it never enters agent state or session history.
                        let normalized = normalize_replacement_content(replacement);
                        // TS replaces the shared message object in place, so agent state, the
                        // agent loop's copy (tool execution, turn_end, agent_end) and persistence
                        // all see the replacement.
                        if normalized != message {
                            self.agent.replace_event_message(normalized.clone());
                        }
                        AgentEvent::MessageEnd {
                            message: normalized,
                        }
                    }
                    None => AgentEvent::MessageEnd { message },
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let _ = runner
                    .emit(ExtensionEvent::ToolExecutionStart(ToolExecutionStartEvent {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        parent_tool_call_id: None,
                    }))
                    .await;
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                }
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                let _ = runner
                    .emit(ExtensionEvent::ToolExecutionUpdate(
                        ToolExecutionUpdateEvent {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: args.clone(),
                            partial_result: serde_json::to_value(&partial_result)
                                .unwrap_or(Value::Null),
                            parent_tool_call_id: None,
                        },
                    ))
                    .await;
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id,
                    tool_name,
                    args,
                    partial_result,
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
            } => {
                let _ = runner
                    .emit(ExtensionEvent::ToolExecutionEnd(ToolExecutionEndEvent {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        result: serde_json::to_value(&result).unwrap_or(Value::Null),
                        is_error,
                        duration_ms,
                        parent_tool_call_id: None,
                    }))
                    .await;
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    result,
                    is_error,
                    duration_ms,
                }
            }
        }
    }

    /// Subscribe to agent events.
    /// Session persistence is handled internally (saves messages on message_end).
    /// Multiple listeners can be added. Returns unsubscribe function for this listener.
    pub fn subscribe(&self, listener: &AgentSessionEventListener) -> Arc<dyn Fn() + Send + Sync> {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.push((id, listener.clone()));
            id
        };
        let weak = self.weak.clone();
        Arc::new(move || {
            if let Some(session) = weak.upgrade() {
                session.state.lock().listeners.retain(|(lid, _)| *lid != id);
            }
        })
    }

    /// Disconnect from agent events during disposal.
    fn disconnect_from_agent(&self) {
        let subscription_taken = self.agent_subscription.lock().take();
        if let Some(subscription) = subscription_taken {
            subscription.unsubscribe();
        }
    }

    /// Remove all listeners and disconnect from agent.
    /// Call this when completely done with the session.
    pub fn dispose(&self) {
        self.abort_retry();
        self.abort_compaction();
        self.abort_branch_summary();
        self.abort_bash();
        self.agent.abort();

        self.runner().invalidate(Some(
            "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().",
        ));
        self.disconnect_from_agent();
        self.state.lock().listeners.clear();
        if let Some(warmer) = &self.cache_warmer {
            warmer.set_on_warmed(None);
            warmer.cancel();
        }
        let _ = cleanup_session_resources(Some(&self.session_id()));
    }

    // =========================================================================
    // Read-only State Access
    // =========================================================================

    /// Refresh the public finalized transcript from the canonical session projection.
    pub fn refresh_context(&self) {
        self.refresh_finalized_context();
    }

    /// Full agent state
    pub fn state(&self) -> AgentState {
        self.agent.state()
    }

    /// Current cache-warming state and the policy inputs that produced it.
    pub fn cache_warming_status(&self) -> Option<CacheWarmingStatus> {
        self.cache_warmer.as_ref().map(|w| w.status())
    }

    /// Persist the cache-warming mode and immediately reconcile active warming.
    pub fn set_cache_warming_mode(&self, mode: &CacheWarmingMode) {
        self.settings_manager.set_cache_warming_mode(mode);
        if let Some(warmer) = &self.cache_warmer {
            warmer.on_mode_changed();
        }
    }

    /// Current model (may be undefined if not yet selected)
    pub fn model(&self) -> Option<Model> {
        // As in TS, this is always the agent's model: pi-agent's placeholder ("unknown" id, api and
        // provider, no reasoning) when none was selected. Callers then report a missing API key for
        // "the selected model", clamp thinking to "off", and RPC state carries the placeholder.
        Some(self.agent.model())
    }

    /// Current thinking level
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.agent.thinking_level()
    }

    /// Under a virtual selection, the physical model and thinking level of the latest successful response.
    pub fn routed_model(&self) -> Option<AgentSessionRoutedModel> {
        let model = self.model()?;
        if !is_virtual_model(&model) {
            return None;
        }
        let latest = find_latest_response(&self.agent.messages())?;
        let physical = self
            .model_runtime
            .get_physical_model(&latest.provider, &latest.model)?;
        Some(AgentSessionRoutedModel {
            model: physical,
            thinking_level: latest.thinking_level,
        })
    }

    /// Whether the session is currently processing an agent run or post-run continuation.
    pub fn is_streaming(&self) -> bool {
        self.state.lock().is_agent_run_active
    }

    /// Whether the session has no active agent run, compaction, branch summary, retry, or queued continuation.
    pub fn is_idle(&self) -> bool {
        let state = self.state.lock();
        !state.is_agent_run_active
            && state.auto_compaction_abort.is_none()
            && state.compaction_abort.is_none()
            && state.branch_summary_abort.is_none()
    }

    /// Current effective system prompt, including changes not yet sent to the model.
    pub fn system_prompt(&self) -> String {
        let options = {
            let state = self.state.lock();
            state
                .run_system_prompt_options
                .clone()
                .unwrap_or_else(|| state.base_system_prompt_options.clone())
        };
        build_system_prompt(&BuildSystemPromptOptions::from(options)).unwrap_or_default()
    }

    /// Replace the message of the `message_end` event being emitted, for listeners. TS listeners
    /// receive the shared message object and may mutate it before it is persisted; this is the
    /// Rust equivalent. Ignored outside a `message_end` emit.
    pub fn replace_message_end_message(&self, message: AgentMessage) {
        self.state.lock().message_end_replacement = Some(message);
    }

    /// Current retry attempt (0 if not retrying)
    pub fn retry_attempt(&self) -> i64 {
        self.state.lock().retry_attempt as i64
    }

    /// Get the names of currently active tools, which are the tools declared to the model.
    /// Tools with `codemode` or `deferred` exposure are callable from other tools without being active.
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.agent.with_state(|s| s.tools.iter().map(|t| t.name().to_string()).collect())
    }

    /// Get the names of the tools that tools can call through `ctx.executeTool()`.
    pub fn get_callable_tool_names(&self) -> Vec<String> {
        self.get_callable_tools(None)
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Get all configured tools with name, description, parameter schema, prompt guidelines, and source metadata.
    pub fn get_all_tools(&self) -> Vec<ToolInfo> {
        let definitions: Vec<ToolDefinitionEntry> =
            self.state.lock().tool_definitions.values().cloned().collect();
        definitions
            .into_iter()
            .map(|ToolDefinitionEntry { definition, source_info }| ToolInfo {
                exposure: self.get_tool_exposure(&definition.name),
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: definition.parameters.clone(),
                prompt_guidelines: definition.prompt_guidelines.clone(),
                reaches_tools: definition.reaches_tools.clone(),
                namespace: definition.namespace.clone(),
                annotations: definition.annotations.clone(),
                source_info,
            })
            .collect()
    }

    pub fn get_tool_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.state
            .lock()
            .tool_definitions
            .get(name)
            .map(|e| e.definition.clone())
    }

    /// Set active tools by name.
    /// Only tools in the registry can be enabled. Unknown and hidden tool names are ignored.
    /// Also rebuilds the system prompt to reflect the new tool set.
    /// Changes take effect on the next agent turn.
    pub fn set_active_tools_by_name(&self, tool_names: &[String]) {
        let previous = self.get_active_tool_names();
        self.set_active_tools(tool_names);
        // A loadout that deactivates a tool replaces the restored one, whose pending tools are
        // dropped. One that only adds tools, like activating tool_search, keeps them.
        let active: HashSet<String> = self.get_active_tool_names().into_iter().collect();
        if previous.iter().any(|name| !active.contains(name)) {
            self.state.lock().pending_tool_names.clear();
        }
    }

    fn set_active_tools(&self, tool_names: &[String]) {
        let tools = self.apply_tool_loadout(tool_names);
        let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        {
            let mut state = self.state.lock();
            for name in &names {
                state.pending_tool_names.shift_remove(name);
            }
        }
        self.rebuild_system_prompt(&names);
    }

    /// Whether `--tools` and `--exclude-tools` keep the tool registered. MCP tools stay registered
    /// unless the allowlist filters them.
    fn is_allowed_tool(&self, name: &str) -> bool {
        if self
            .excluded_tools
            .as_ref()
            .is_some_and(|excluded| excluded(name.to_string()))
        {
            return false;
        }
        match &self.allowed_tools {
            None => true,
            Some(allowed) if allowed(name.to_string()) => true,
            Some(_) => !self.allowlist_filters_mcp && is_mcp_tool_name(name),
        }
    }

    /// Whether the tool may be active, which declares it to the model.
    fn is_activatable(&self, name: &str) -> bool {
        match &self.allowed_tools {
            None => true,
            Some(allowed) if allowed(name.to_string()) || !is_mcp_tool_name(name) => true,
            Some(_) => {
                self.get_tool_exposure(name) != ToolExposure::Direct
                    && self.state.lock().tool_registry.contains_key("tool_search")
            }
        }
    }

    fn get_tool_exposure(&self, name: &str) -> ToolExposure {
        self.state
            .lock()
            .tool_definitions
            .get(name)
            .map(|e| e.definition.effective_exposure())
            .unwrap_or(ToolExposure::Direct)
    }

    /// Tools callable through `ctx.executeTool()`: the active `direct` tools and every registered
    /// `codemode` or `deferred` tool.
    fn get_callable_tools(&self, active: Option<&HashSet<String>>) -> Vec<AgentToolRef> {
        let owned;
        let active = match active {
            Some(active) => active,
            None => {
                owned = self.get_active_tool_names().into_iter().collect();
                &owned
            }
        };
        let registry: Vec<AgentToolRef> = self.state.lock().tool_registry.values().cloned().collect();
        registry
            .into_iter()
            .filter(|tool| {
                let exposure = self.get_tool_exposure(tool.name());
                exposure == ToolExposure::Codemode
                    || exposure == ToolExposure::Deferred
                    || (exposure == ToolExposure::Direct && active.contains(tool.name()))
            })
            .collect()
    }

    /// Set the agent's tools for the given active tool names and return them.
    fn apply_tool_loadout(&self, tool_names: &[String]) -> Vec<AgentToolRef> {
        let mut seen = HashSet::new();
        let registry = self.state.lock().tool_registry.clone();
        let tools: Vec<AgentToolRef> = tool_names
            .iter()
            .filter(|name| seen.insert((*name).clone()))
            .filter_map(|name| {
                let tool = registry.get(name)?;
                (self.get_tool_exposure(name) != ToolExposure::Hidden && self.is_activatable(name))
                    .then(|| tool.clone())
            })
            .collect();
        let hooks: Vec<ToolDefinitionEntry> = {
            let state = self.state.lock();
            tools
                .iter()
                .filter_map(|tool| state.tool_definitions.get(tool.name()))
                .filter(|entry| entry.definition.prepare_loadout.is_some())
                .cloned()
                .collect()
        };
        let mut hidden = HashSet::new();
        let mut declared = tools.clone();
        if !hooks.is_empty() {
            let active: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
            let (definitions_snapshot, guidelines_snapshot) = {
                let state = self.state.lock();
                (
                    state.tool_definitions.clone(),
                    state.tool_prompt_guidelines.clone(),
                )
            };
            let loadout = SessionToolLoadout {
                declared: tools.clone(),
                callable: self.get_callable_tools(Some(&active)),
                registered: registry.values().cloned().collect(),
                definitions: definitions_snapshot,
                guidelines: guidelines_snapshot,
            };
            let mut descriptions: IndexMap<String, String> = IndexMap::new();
            for ToolDefinitionEntry { definition, source_info } in &hooks {
                let Some(prepare) = &definition.prepare_loadout else {
                    continue;
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(&loadout)
                }));
                match result {
                    Ok(changes) => {
                        if let Some(changes) = changes {
                            for (name, description) in changes.descriptions.unwrap_or_default() {
                                descriptions.insert(name, description);
                            }
                            for name in changes.hidden_declarations.unwrap_or_default() {
                                hidden.insert(name);
                            }
                        }
                    }
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "prepareLoadout failed".to_string());
                        self.runner().emit_error(&ExtensionError {
                            extension_path: source_info.path.clone(),
                            event: "prepare_loadout".into(),
                            error: message,
                            stack: None,
                        });
                    }
                }
            }
            declared = tools
                .iter()
                .map(|tool| match descriptions.get(tool.name()) {
                    Some(description) => Arc::new(DescribedTool {
                        inner: tool.clone(),
                        description: description.clone(),
                    }) as AgentToolRef,
                    None => tool.clone(),
                })
                .collect();
        }
        self.state.lock().hidden_declarations = hidden;
        self.agent.set_tools(declared.clone());
        declared
    }

    /// Whether compaction or branch summarization is currently running
    pub fn is_compacting(&self) -> bool {
        let state = self.state.lock();
        state.auto_compaction_abort.is_some()
            || state.compaction_abort.is_some()
            || state.branch_summary_abort.is_some()
    }

    /// All messages including custom types like BashExecutionMessage
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.agent.messages()
    }

    /// Current steering mode
    pub fn steering_mode(&self) -> AgentSessionSteeringMode {
        self.agent.steering_mode()
    }

    /// Current follow-up mode
    pub fn follow_up_mode(&self) -> AgentSessionSteeringMode {
        self.agent.follow_up_mode()
    }

    /// Current session file path, or undefined if sessions are disabled
    pub fn session_file(&self) -> Option<String> {
        self.session_manager.get_session_file()
    }

    /// Current session ID
    pub fn session_id(&self) -> String {
        self.session_manager.get_session_id()
    }

    /// Current session display name, if set
    pub fn session_name(&self) -> Option<String> {
        self.session_manager.get_session_name()
    }

    /// Scoped models for cycling (from --models flag)
    pub fn scoped_models(&self) -> Vec<AgentSessionScopedModels> {
        self.state.lock().scoped_models.clone()
    }

    /// Update scoped models for cycling
    pub fn set_scoped_models(&self, scoped_models: &[AgentSessionSetScopedModelsScopedModels]) {
        self.state.lock().scoped_models = scoped_models
            .iter()
            .map(|m| AgentSessionScopedModels {
                model: m.model.clone(),
                thinking_level: m.thinking_level,
            })
            .collect();
    }

    /// File-based prompt templates
    pub fn prompt_templates(&self) -> Vec<PromptTemplate> {
        self.resource_loader.get_prompts().prompts
    }

    fn normalize_prompt_snippet(text: Option<&str>) -> Option<String> {
        let text = text?;
        if text.is_empty() {
            return None;
        }
        let one_line = text
            .split(['\r', '\n'])
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let one_line = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
        (!one_line.is_empty()).then_some(one_line)
    }

    fn normalize_prompt_guidelines(guidelines: Option<&[String]>) -> Vec<String> {
        let mut unique: IndexSet<String> = IndexSet::new();
        for guideline in guidelines.unwrap_or_default() {
            let normalized = guideline.trim();
            if !normalized.is_empty() {
                unique.insert(normalized.to_string());
            }
        }
        unique.into_iter().collect()
    }

    fn rebuild_system_prompt(&self, tool_names: &[String]) {
        let (valid_tool_names, tool_snippets, hidden, guidelines) = {
            let state = self.state.lock();
            let valid: Vec<String> = tool_names
                .iter()
                .filter(|name| state.tool_registry.contains_key(*name))
                .cloned()
                .collect();
            let mut snippets = IndexMap::new();
            for name in state.tool_registry.keys() {
                // Tools without a snippet are not listed.
                if let Some(snippet) = state.tool_prompt_snippets.get(name) {
                    snippets.insert(name.clone(), snippet.clone());
                }
            }
            (
                valid,
                snippets,
                state.hidden_declarations.iter().cloned().collect::<Vec<_>>(),
                state.tool_prompt_guidelines.clone(),
            )
        };
        let loader_system_prompt = self.resource_loader.get_system_prompt();
        let loader_append = self.resource_loader.get_append_system_prompt();
        let append_system_prompt = if loader_append.is_empty() {
            String::new()
        } else {
            loader_append.join("\n\n")
        };
        let skills = self.resource_loader.get_skills().skills;
        let context_files = self
            .resource_loader
            .get_agents_files()
            .agents_files
            .into_iter()
            .map(|f| BuildSystemPromptOptionsContextFiles {
                path: f.path,
                content: f.content,
            })
            .collect();
        let options = normalize_build_system_prompt_options(&BuildSystemPromptOptions {
            custom_prompt: loader_system_prompt,
            force_system_prompt: None,
            selected_tools: Some(valid_tool_names),
            hidden_tools: Some(hidden),
            tool_snippets: Some(tool_snippets),
            tool_guidelines: Some(guidelines),
            prompt_guidelines: None,
            append_system_prompt: Some(append_system_prompt),
            sections: None,
            cwd: self.cwd.clone(),
            context_files: Some(context_files),
            skills: Some(skills),
        });
        self.state.lock().base_system_prompt_options = options;
    }

    /// Apply a prompt and tool loadout for the next request. Sets the executable tools and
    /// returns a system message patching the prompt sections the model currently has (replayed
    /// from `messages`), or `None` when the prompt is unchanged.
    fn prepare_prompt_and_tool_loadout(
        &self,
        options: &mut NormalizedBuildSystemPromptOptions,
        messages: Option<&[AgentMessage]>,
    ) -> anyhow::Result<Option<SystemMessage>> {
        options.selected_tools = self
            .apply_tool_loadout(&options.selected_tools)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        // The tool list and rules must match the declarations the request carries.
        options.hidden_tools = self.state.lock().hidden_declarations.iter().cloned().collect();
        let owned;
        let messages = match messages {
            Some(messages) => messages,
            None => {
                owned = self.agent.messages();
                &owned
            }
        };
        let previous = get_current_system_message(messages)
            .and_then(|m| m.sections)
            .unwrap_or_default();
        let sections = diff_system_prompt_sections(
            previous,
            &build_normalized_system_prompt_sections(options)?,
        );
        Ok(sections.map(|sections| SystemMessage {
            content: SystemContent::Text(String::new()),
            sections: Some(sections),
            tools_added: None,
            tools_removed: None,
            timestamp: now_ms(),
            timestamp_last: false,
            ..Default::default()
        }))
    }

    /// Remove the declarations that `prepareLoadout` hooks hide from every request.
    fn install_hidden_declarations_projection(&self) {
        let previous = self.agent.transform_context();
        let weak = self.weak.clone();
        self.agent
            .set_transform_context(Some(Arc::new(move |messages, signal| {
                let previous = previous.clone();
                let weak = weak.clone();
                Box::pin(async move {
                    let transformed = match &previous {
                        Some(previous) => previous(messages, signal).await,
                        None => messages,
                    };
                    let Some(session) = weak.upgrade() else {
                        return transformed;
                    };
                    let hidden = session.state.lock().hidden_declarations.clone();
                    if hidden.is_empty() {
                        return transformed;
                    }
                    transformed
                        .into_iter()
                        .map(|message| match message {
                            AgentMessage::System(mut system)
                                if system.tools_added.is_some() || system.tools_removed.is_some() =>
                            {
                                let added: Vec<_> = system
                                    .tools_added
                                    .take()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|t| !hidden.contains(&t.name))
                                    .collect();
                                let removed: Vec<_> = system
                                    .tools_removed
                                    .take()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|t| !hidden.contains(&t.name))
                                    .collect();
                                system.tools_added = (!added.is_empty()).then_some(added);
                                system.tools_removed = (!removed.is_empty()).then_some(removed);
                                // `{ ...rest, toolsAdded?, toolsRemoved? }`
                                system.timestamp_last = false;
                                AgentMessage::System(system)
                            }
                            other => other,
                        })
                        .collect()
                })
            })));
    }

    /// Send a forced prompt as the provider's leading system prompt without recording it.
    fn install_agent_forced_prompt_projection(&self) {
        let previous = self.agent.transform_context();
        let weak = self.weak.clone();
        self.agent
            .set_transform_context(Some(Arc::new(move |messages, signal| {
                let previous = previous.clone();
                let weak = weak.clone();
                Box::pin(async move {
                    let transformed = match &previous {
                        Some(previous) => previous(messages, signal).await,
                        None => messages,
                    };
                    let Some(session) = weak.upgrade() else {
                        return transformed;
                    };
                    let forced = session
                        .state
                        .lock()
                        .run_system_prompt_options
                        .as_ref()
                        .and_then(|o| o.force_system_prompt.clone());
                    let Some(forced) = forced else {
                        return transformed;
                    };
                    let current = get_current_system_message(&transformed);
                    let head = SystemMessage {
                        content: SystemContent::Text(forced),
                        sections: None,
                        tools_added: current.as_ref().and_then(|c| c.tools_added.clone()),
                        tools_removed: None,
                        timestamp: current.as_ref().map(|c| c.timestamp).unwrap_or_else(now_ms),
                        // `{ role, content, toolsAdded?, timestamp }`
                        timestamp_last: true,
                        ..Default::default()
                    };
                    let mut result = vec![AgentMessage::System(head)];
                    result.extend(
                        transformed
                            .into_iter()
                            .filter(|m| !matches!(m, AgentMessage::System(_))),
                    );
                    result
                })
            })));
    }

    /// Restore the active tool loadout declared by the session transcript, if it declares one.
    fn restore_tools_from_transcript(&self, modifiers: &[String]) {
        self.state.lock().pending_tool_names.clear();
        // All system messages on the branch, including those before a compaction.
        let system_messages: Vec<AgentMessage> = self
            .session_manager
            .get_branch(None)
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(e) if matches!(e.message, AgentMessage::System(_)) => {
                    Some(e.message)
                }
                _ => None,
            })
            .collect();
        // No recorded loadout: keep the current tools.
        if system_messages.is_empty() {
            return;
        }
        // Tools the branch ever added or removed. Removed tools stay inactive.
        let mut mentioned: HashSet<String> = HashSet::new();
        for message in &system_messages {
            if let AgentMessage::System(system) = message {
                for tool in system.tools_added.iter().flatten() {
                    mentioned.insert(tool.name.clone());
                }
                for tool in system.tools_removed.iter().flatten() {
                    mentioned.insert(tool.name.clone());
                }
            }
        }
        // Extension tools the branch never mentioned were registered after it recorded its
        // loadout, so they are activated like newly registered tools.
        let (registry_names, base_names): (Vec<String>, HashSet<String>) = {
            let state = self.state.lock();
            (
                state.tool_registry.keys().cloned().collect(),
                state.base_tool_definitions.keys().cloned().collect(),
            )
        };
        let new_tools: Vec<String> = registry_names
            .into_iter()
            .filter(|name| {
                !mentioned.contains(name)
                    && !base_names.contains(name)
                    && self.is_activated_on_registration(name)
            })
            .collect();
        // The recorded loadout plus the new tools, with `--tools +name`/`-name` applied.
        let mut base: Vec<String> = pi_agent::transcript::get_current_tools(&system_messages)
            .into_iter()
            .map(|t| t.name)
            .collect();
        base.extend(new_tools);
        let names = apply_tool_modifiers(&base, modifiers);
        // Recorded tools that are not registered yet are activated when they register.
        let pending: IndexSet<String> = names
            .iter()
            .filter(|name| self.is_allowed_tool(name))
            .cloned()
            .collect();
        self.state.lock().pending_tool_names = pending;
        // Ignores names that are not registered.
        self.set_active_tools(&names);
    }

    // =========================================================================
    // Prompting
    // =========================================================================

    /// Start a run from a user or custom message. The session counts as busy from here on, so
    /// messages sent while before_agent_start handlers run are queued into this run instead of
    /// starting another, and an abort requested meanwhile aborts the run as soon as it exists.
    fn start_run(
        self: Arc<Self>,
        trigger: BeforeAgentStartTrigger,
        on_started: Option<Box<dyn FnOnce() + Send>>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            {
                let mut state = self.state.lock();
                state.is_agent_run_active = true;
                state.agent_run_abort_requested = false;
            }
            let messages = match self.prepare_run(trigger).await {
                Ok(messages) => messages,
                Err(error) => {
                    self.state.lock().is_agent_run_active = false;
                    self.flush_pending_bash_messages();
                    self.flush_pending_custom_messages();
                    self.resolve_idle_wait_if_idle();
                    return Err(error);
                }
            };
            // Messages held while the run was starting go before its starting messages.
            self.flush_pending_bash_messages();
            self.flush_pending_custom_messages();
            if let Some(on_started) = on_started {
                on_started();
            }

            // Compaction before the prompt may have scheduled a retry; the new prompt replaces it.
            self.state.lock().failed_response = None;
            self.record_selection();
            // The run records the loadout in the transcript; restored tools that did not register
            // by now are dropped, so a tool that never registers does not stay pending.
            self.state.lock().pending_tool_names.clear();
            let run_result = self.run_loop(messages).await;

            let abort_requested = self.state.lock().agent_run_abort_requested;
            if abort_requested {
                self.finish_cancelled_retry();
            }
            {
                let mut state = self.state.lock();
                state.failed_response = None;
                state.run_system_prompt_options = None;
            }
            self.flush_pending_bash_messages();
            self.flush_pending_custom_messages();
            let settled = self.emit_agent_settled().await;
            run_result?;
            settled
        })
    }

    async fn run_loop(&self, messages: Vec<AgentMessage>) -> anyhow::Result<()> {
        self.agent.prompt(messages).await?;
        loop {
            if self.state.lock().agent_run_abort_requested {
                break;
            }
            if self.handle_post_agent_run().await? {
                if self.state.lock().agent_run_abort_requested {
                    break;
                }
                self.agent.continue_run().await?;
                continue;
            }
            let abort_requested = self.state.lock().agent_run_abort_requested;
            if abort_requested || !self.run_before_settle_boundary().await {
                break;
            }
            if self.state.lock().agent_run_abort_requested {
                break;
            }
            self.agent.continue_run().await?;
        }
        Ok(())
    }

    /// Prepare a run and return its starting messages.
    async fn prepare_run(&self, trigger: BeforeAgentStartTrigger) -> anyhow::Result<Vec<AgentMessage>> {
        // Check if we need to compact before sending (catches aborted responses).
        if let Some(last_assistant) = self.find_last_assistant_message() {
            self.check_compaction(&last_assistant, false, &[]).await;
        }

        let base_options = self.state.lock().base_system_prompt_options.clone();
        let selected_tools_before = base_options.selected_tools.clone();
        let mut result = self
            .runner()
            .emit_before_agent_start(&trigger, &BuildSystemPromptOptions::from(&base_options))
            .await?;
        // Handlers may edit selectedTools or call setActiveTools(), which updates the live
        // loadout instead. An explicit edit wins; otherwise the live loadout is authoritative.
        let handler_edited_tools = result.system_prompt_options.selected_tools != selected_tools_before;
        if !handler_edited_tools {
            result.system_prompt_options.selected_tools = self.get_active_tool_names();
        }

        let mut messages: Vec<AgentMessage> = Vec::new();
        match &trigger {
            BeforeAgentStartKind::Custom { message } => {
                messages.push(custom_message_to_agent(message));
            }
            BeforeAgentStartKind::User { prompt, images } => {
                // Normalize images after before_agent_start so extension-driven model selection
                // determines the resize profile used for the request and history.
                let (normalized_images, hints) =
                    self.normalize_prompt_images(images.as_deref()).await?;
                let text = if hints.is_empty() {
                    prompt.clone()
                } else {
                    format!("{prompt}\n\n{}", hints.join("\n"))
                };
                let mut content = vec![UserContentBlock::Text(TextContent::new(text))];
                content.extend(normalized_images.into_iter().map(UserContentBlock::Image));
                messages.push(AgentMessage::User(UserMessage {
                    content: UserContent::Blocks(content),
                    timestamp: now_ms(),
                    ..Default::default()
                }));
            }
        }
        // Inject any pending "nextTurn" messages as context alongside the starting message
        let next_turn = std::mem::take(&mut self.state.lock().pending_next_turn_messages);
        messages.extend(next_turn.iter().map(custom_message_to_agent));
        for msg in &result.messages {
            messages.push(custom_message_to_agent(&CustomMessage {
                role: CUSTOM_ROLE.to_string(),
                custom_type: msg.custom_type.clone(),
                content: msg.content.clone(),
                display: msg.display,
                details: msg.details.clone(),
                timestamp: now_ms(),
            }));
        }
        let mut options = result.system_prompt_options;
        let update_message = self.prepare_prompt_and_tool_loadout(&mut options, None)?;
        self.state.lock().run_system_prompt_options = Some(options);
        if let Some(update) = update_message {
            messages.insert(0, AgentMessage::System(update));
        }
        Ok(messages)
    }

    async fn handle_post_agent_run(&self) -> anyhow::Result<bool> {
        let (message, tool_results, abort_requested) = {
            let mut state = self.state.lock();
            (
                state.last_assistant_message.take(),
                std::mem::take(&mut state.last_assistant_tool_results),
                state.agent_run_abort_requested,
            )
        };
        if abort_requested {
            self.finish_cancelled_retry();
            return Ok(false);
        }
        let Some(message) = message else {
            return Ok(self.agent.has_queued_messages());
        };

        if self.is_retryable_error(&message) && self.prepare_retry(&message).await {
            let aborted = self.state.lock().agent_run_abort_requested;
            if aborted {
                self.finish_cancelled_retry();
            }
            self.state.lock().failed_response = Some(message);
            return Ok(!aborted);
        }
        if self.state.lock().agent_run_abort_requested {
            self.finish_cancelled_retry();
            return Ok(false);
        }

        let retry_attempt = self.state.lock().retry_attempt;
        if message.stop_reason == StopReason::Error && retry_attempt > 0 {
            self.emit(AgentSessionEvent::AutoRetryEnd {
                success: false,
                attempt: retry_attempt,
                final_error: message.error_message.clone(),
            });
            self.state.lock().retry_attempt = 0;
        }

        if self.check_compaction(&message, true, &tool_results).await {
            return Ok(!self.state.lock().agent_run_abort_requested);
        }

        // The low-level loop drains both queues before agent_end. Messages queued by
        // agent_end handlers require a fresh run before pre-settlement handlers fire.
        Ok(!self.state.lock().agent_run_abort_requested && self.agent.has_queued_messages())
    }

    async fn run_before_settle_boundary(&self) -> bool {
        let runner = self.runner();
        if !runner.has_handlers("agent_before_settle") {
            return self.agent.has_queued_messages();
        }
        let outcome = {
            let mut state = self.state.lock();
            state.is_before_settle = true;
            state.abort_during_before_settle = false;
            state.last_activity_outcome
        };
        let result = runner
            .emit_boundary(
                &BoundaryBaseEvent::AgentBeforeSettle { outcome },
                self.boundary_context_builder(BoundaryKind::AgentBeforeSettle),
            )
            .await;
        let decision = match result {
            Ok(result) => {
                self.commit_boundary_drafts(&result.entries);
                self.flush_pending_custom_messages();
                let final_context =
                    self.build_boundary_context(&[], BoundaryKind::AgentBeforeSettle);
                if self.state.lock().abort_during_before_settle {
                    false
                } else {
                    let should_continue = result.r#continue || self.agent.has_queued_messages();
                    let can_continue = final_context.map(|c| c.can_continue).unwrap_or(false);
                    if should_continue && !can_continue {
                        if result.r#continue {
                            self.report_invalid_boundary_continuation("agent_before_settle");
                        }
                        false
                    } else {
                        should_continue
                    }
                }
            }
            Err(_) => false,
        };
        self.state.lock().is_before_settle = false;
        decision
    }

    async fn run_input_handlers(
        &self,
        text: &str,
        images: Option<Vec<ImageContent>>,
        source: InputSource,
        streaming_behavior: Option<PromptOptionsStreamingBehavior>,
    ) -> anyhow::Result<Option<(String, Option<Vec<ImageContent>>)>> {
        let runner = self.runner();
        if !runner.has_handlers("input") {
            return Ok(Some((text.to_string(), images)));
        }
        let behavior = streaming_behavior.map(|b| match b {
            PromptOptionsStreamingBehavior::Steer => StreamingBehavior::Steer,
            PromptOptionsStreamingBehavior::FollowUp => StreamingBehavior::FollowUp,
        });
        let result = runner
            .emit_input(text, images.clone(), source, behavior)
            .await?;
        Ok(match result {
            InputEventResult::Handled => None,
            InputEventResult::Transform {
                text,
                images: new_images,
            } => Some((text, new_images.or(images))),
            InputEventResult::Continue => Some((text.to_string(), images)),
        })
    }

    async fn normalize_prompt_images(
        &self,
        images: Option<&[ImageContent]>,
    ) -> anyhow::Result<(Vec<ImageContent>, Vec<String>)> {
        let Some(images) = images else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut normalized = Vec::new();
        let mut hints = Vec::new();
        let resize_options = self
            .limits_model()
            .and_then(|m| m.input_limits)
            .and_then(|l| l.images)
            .and_then(|i| i.resize)
            .and_then(|r| serde_json::to_value(r).ok())
            .and_then(|v| serde_json::from_value(v).ok());
        for image in images {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.data.as_bytes())
                .unwrap_or_default();
            let processed = process_image(
                &bytes,
                &image.mime_type,
                Some(&ProcessImageOptions {
                    auto_resize_images: Some(self.settings_manager.get_image_auto_resize()),
                    resize_options: resize_options.clone(),
                }),
            )
            .await?;
            match processed {
                ProcessImageResult::ProcessImageResultVariant1(ok) if ok.ok => {
                    normalized.push(ImageContent { data: ok.data, mime_type: ok.mime_type });
                    hints.extend(ok.hints);
                }
                ProcessImageResult::ProcessImageResultVariant2(failed) => {
                    hints.push(failed.message);
                }
                ProcessImageResult::ProcessImageResultVariant1(_) => {}
            }
        }
        Ok((normalized, hints))
    }

    /// Send a prompt to the agent.
    /// - Handles extension commands (registered via pi.registerCommand) immediately, even during streaming
    /// - Expands file-based prompt templates by default
    /// - During streaming, queues via steer() or followUp() based on streamingBehavior option
    /// - Validates model and API key before sending (when not streaming)
    ///
    /// @throws Error if streaming and no streamingBehavior specified
    /// @throws Error if no model selected or no API key available (when not streaming)
    pub async fn prompt(&self, text: &str, options: Option<&PromptOptions>) -> anyhow::Result<()> {
        self.arc()
            .prompt_owned(text.to_string(), options.cloned().unwrap_or_default())
            .await
    }

    fn prompt_owned(
        self: Arc<Self>,
        text: String,
        options: PromptOptions,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            {
                let mut state = self.state.lock();
                if state.is_emitting_agent_settled {
                    let deferred_text = text.clone();
                    let deferred_options = options.clone();
                    state.deferred_settled_actions.push(Box::new(move |session| {
                        session.prompt_owned(deferred_text, deferred_options)
                    }));
                    return Ok(());
                }
            }
            let expand_prompt_templates = options.expand_prompt_templates.unwrap_or(true);
            let preflight_result = options.preflight_result.clone();
            // Handle extension commands first (execute immediately, even during streaming)
            if expand_prompt_templates
                && text.starts_with('/')
                && self.try_execute_extension_command(&text).await
            {
                if let Some(preflight) = &preflight_result {
                    preflight(PromptDisposition::Handled);
                }
                return Ok(());
            }

            if self.state.lock().compaction_abort.is_some() {
                anyhow::bail!(
                    "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry."
                );
            }

            // Emit input event for extension interception (before skill/template expansion)
            let streaming = self.is_streaming();
            let processed = self
                .run_input_handlers(
                    &text,
                    options.images.clone(),
                    options.source.unwrap_or(InputSource::Interactive),
                    if streaming {
                        options.streaming_behavior
                    } else {
                        None
                    },
                )
                .await?;
            let Some((current_text, current_images)) = processed else {
                if let Some(preflight) = &preflight_result {
                    preflight(PromptDisposition::Handled);
                }
                return Ok(());
            };

            // Expand skill commands (/skill:name args) and prompt templates (/template args)
            let mut expanded_text = current_text;
            if expand_prompt_templates {
                expanded_text = self.expand_skill_command(&expanded_text);
                expanded_text = expand_prompt_template(&expanded_text, &self.prompt_templates());
            }

            // If streaming, queue via steer() or followUp() based on option
            if self.is_streaming() {
                let Some(behavior) = options.streaming_behavior else {
                    anyhow::bail!(
                        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                    );
                };
                match behavior {
                    PromptOptionsStreamingBehavior::FollowUp => {
                        self.queue_follow_up(&expanded_text, current_images)
                    }
                    PromptOptionsStreamingBehavior::Steer => {
                        self.queue_steer(&expanded_text, current_images)
                    }
                }
                if let Some(preflight) = &preflight_result {
                    preflight(PromptDisposition::Queued);
                }
                return Ok(());
            }

            // Validate model
            let Some(model) = self.model() else {
                anyhow::bail!("{}", format_no_model_selected_message());
            };

            let has_configured_auth = self.model_runtime.has_configured_auth(&model.provider)
                || self
                    .model_runtime
                    .check_auth(&model.provider, None)
                    .await?
                    .is_some();
            if !has_configured_auth {
                if self.model_runtime.is_using_o_auth(&model.provider) {
                    anyhow::bail!(
                        "Authentication failed for \"{0}\". Credentials may have expired or network is unavailable. Run '/login {0}' to re-authenticate.",
                        model.provider
                    );
                }
                anyhow::bail!("{}", format_no_api_key_found_message(&model.provider));
            }

            let on_started: Option<Box<dyn FnOnce() + Send>> = preflight_result.map(|preflight| {
                Box::new(move || preflight(PromptDisposition::Started)) as Box<dyn FnOnce() + Send>
            });
            self.clone()
                .start_run(
                    BeforeAgentStartKind::User {
                        prompt: expanded_text,
                        images: current_images,
                    },
                    on_started,
                )
                .await
        })
    }

    /// Try to execute an extension command. Returns true if command was found and executed.
    async fn try_execute_extension_command(&self, text: &str) -> bool {
        let (command_name, args) = match text.find(' ') {
            Some(i) => (&text[1..i], &text[i + 1..]),
            None => (&text[1..], ""),
        };
        let runner = self.runner();
        let Some(command) = runner.get_command(command_name) else {
            return false;
        };
        // Get command context from extension runner (includes session control methods)
        let ctx = runner.create_command_context();
        if let Err(error) = (command.handler)(args.to_string(), ctx).await {
            runner.emit_error(&ExtensionError {
                extension_path: format!("command:{command_name}"),
                event: "command".into(),
                error: error.to_string(),
                stack: None,
            });
        }
        true
    }

    /// Expand skill commands (/skill:name args) to their full content.
    fn expand_skill_command(&self, text: &str) -> String {
        if !text.starts_with("/skill:") {
            return text.to_string();
        }
        let (skill_name, args) = match text.find(' ') {
            Some(i) => (&text[7..i], text[i + 1..].trim()),
            None => (&text[7..], ""),
        };
        let skills = self.resource_loader.get_skills().skills;
        let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
            return text.to_string(); // Unknown skill, pass through
        };
        let content = std::fs::read_to_string(&skill.file_path)
            .map_err(anyhow::Error::from)
            .and_then(|content| strip_frontmatter(&content));
        match content {
            Ok(content) => {
                let body = content.trim();
                let skill_block = format!(
                    "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                    skill.name, skill.file_path, skill.base_dir, body
                );
                if args.is_empty() {
                    skill_block
                } else {
                    format!("{skill_block}\n\n{args}")
                }
            }
            Err(error) => {
                self.runner().emit_error(&ExtensionError {
                    extension_path: skill.file_path.clone(),
                    event: "skill_expansion".into(),
                    error: error.to_string(),
                    stack: None,
                });
                text.to_string()
            }
        }
    }

    async fn queue_user_input(
        &self,
        text: &str,
        images: Option<Vec<ImageContent>>,
        behavior: PromptOptionsStreamingBehavior,
        source: InputSource,
    ) -> anyhow::Result<QueuedInputDisposition> {
        if text.starts_with('/') {
            self.throw_if_extension_command(text)?;
        }
        let streaming = self.is_streaming();
        let processed = self
            .run_input_handlers(text, images, source, streaming.then_some(behavior))
            .await?;
        let Some((text, images)) = processed else {
            return Ok(QueuedInputDisposition::Handled);
        };
        let expanded = self.expand_skill_command(&text);
        let expanded = expand_prompt_template(&expanded, &self.prompt_templates());
        match behavior {
            PromptOptionsStreamingBehavior::Steer => self.queue_steer(&expanded, images),
            PromptOptionsStreamingBehavior::FollowUp => self.queue_follow_up(&expanded, images),
        }
        Ok(QueuedInputDisposition::Queued)
    }

    /// Queue a steering message while the agent is running.
    /// Delivered after the current assistant turn finishes executing its tool calls,
    /// before the next LLM call.
    /// Expands skill commands and prompt templates. Errors on extension commands.
    /// @param images Optional image attachments to include with the message
    /// @param options Input source; defaults to interactive
    /// @throws Error if text is an extension command
    pub async fn steer(
        &self,
        text: &str,
        images: Option<&[ImageContent]>,
        options: Option<&AgentSessionSteerOptions>,
    ) -> anyhow::Result<QueuedInputDisposition> {
        self.queue_user_input(
            text,
            images.map(<[ImageContent]>::to_vec),
            PromptOptionsStreamingBehavior::Steer,
            options
                .and_then(|o| o.source)
                .unwrap_or(InputSource::Interactive),
        )
        .await
    }

    /// Queue a follow-up message to be processed after the agent finishes.
    /// Delivered only when agent has no more tool calls or steering messages.
    /// Expands skill commands and prompt templates. Errors on extension commands.
    /// @param images Optional image attachments to include with the message
    /// @param options Input source; defaults to interactive
    /// @throws Error if text is an extension command
    pub async fn follow_up(
        &self,
        text: &str,
        images: Option<&[ImageContent]>,
        options: Option<&AgentSessionFollowUpOptions>,
    ) -> anyhow::Result<QueuedInputDisposition> {
        self.queue_user_input(
            text,
            images.map(<[ImageContent]>::to_vec),
            PromptOptionsStreamingBehavior::FollowUp,
            options
                .and_then(|o| o.source)
                .unwrap_or(InputSource::Interactive),
        )
        .await
    }

    fn user_message_with_images(text: &str, images: Option<Vec<ImageContent>>) -> UserMessage {
        let mut content = vec![UserContentBlock::Text(TextContent::new(text.to_string()))];
        if let Some(images) = images {
            content.extend(images.into_iter().map(UserContentBlock::Image));
        }
        UserMessage {
            content: UserContent::Blocks(content),
            timestamp: now_ms(),
            ..Default::default()
        }
    }

    /// Internal: Queue a steering message (already expanded, no extension command check).
    fn queue_steer(&self, text: &str, images: Option<Vec<ImageContent>>) {
        self.state.lock().steering_messages.push(text.to_string());
        self.emit_queue_update();
        self.agent
            .steer(AgentMessage::User(Self::user_message_with_images(text, images)));
    }

    /// Internal: Queue a follow-up message (already expanded, no extension command check).
    fn queue_follow_up(&self, text: &str, images: Option<Vec<ImageContent>>) {
        self.state.lock().follow_up_messages.push(text.to_string());
        self.emit_queue_update();
        self.agent
            .follow_up(AgentMessage::User(Self::user_message_with_images(text, images)));
    }

    /// Throw an error if the text is an extension command.
    fn throw_if_extension_command(&self, text: &str) -> anyhow::Result<()> {
        let command_name = match text.find(' ') {
            Some(i) => &text[1..i],
            None => &text[1..],
        };
        if self.runner().get_command(command_name).is_some() {
            anyhow::bail!(
                "Extension command \"/{command_name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            );
        }
        Ok(())
    }

    /// Send a custom message to the session. Creates a CustomMessageEntry.
    ///
    /// Handles four cases:
    /// - Streaming: queues message, processed when loop pulls from queue
    /// - Streaming + triggerTurn false: appended to state/session once the current turn ends
    /// - Not streaming + triggerTurn: appends to state/session, starts new turn
    /// - Not streaming + no trigger: appends to state/session, no turn
    ///
    /// @param message Custom message with customType, content, display, details
    /// @param options.triggerTurn If true and not streaming, triggers a new LLM turn
    /// @param options.deliverAs Delivery mode: "steer", "followUp", or "nextTurn"
    pub async fn send_custom_message(
        &self,
        message: &AgentSessionSendCustomMessageMessage,
        options: Option<&AgentSessionSendCustomMessageOptions>,
    ) -> anyhow::Result<()> {
        self.arc()
            .send_custom_message_owned(message.clone(), options.cloned().unwrap_or_default())
            .await
    }

    fn send_custom_message_owned(
        self: Arc<Self>,
        message: AgentSessionSendCustomMessageMessage,
        options: AgentSessionSendCustomMessageOptions,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            let app_message = CustomMessage {
                role: CUSTOM_ROLE.to_string(),
                custom_type: message.custom_type.clone(),
                content: message.content.clone(),
                display: message.display,
                details: message.details.clone(),
                timestamp: now_ms(),
            };
            let streaming = self.is_streaming();
            if options.deliver_as == Some(AgentSessionSendCustomMessageOptionsDeliverAs::NextTurn) {
                self.state.lock().pending_next_turn_messages.push(app_message);
            } else if streaming && options.trigger_turn != Some(false) {
                if options.deliver_as == Some(AgentSessionSendCustomMessageOptionsDeliverAs::FollowUp) {
                    self.agent.follow_up(custom_message_to_agent(&app_message));
                } else {
                    self.agent.steer(custom_message_to_agent(&app_message));
                }
            } else if options.trigger_turn == Some(true) {
                {
                    let mut state = self.state.lock();
                    if state.is_emitting_agent_settled {
                        let message = message.clone();
                        let options = options.clone();
                        state.deferred_settled_actions.push(Box::new(move |session| {
                            session.send_custom_message_owned(message, options)
                        }));
                        return Ok(());
                    }
                }
                self.clone()
                    .start_run(
                        BeforeAgentStartKind::Custom {
                            message: app_message,
                        },
                        None,
                    )
                    .await?;
            } else if streaming {
                // Appending now would put the message between an assistant tool call and its
                // result, which providers that validate message order reject on replay. Defer
                // to the end of the turn.
                self.state.lock().pending_custom_messages.push(app_message);
            } else {
                self.append_custom_message(&app_message);
            }
            Ok(())
        })
    }

    fn append_custom_message(&self, app_message: &CustomMessage) {
        let _ = self.session_manager.append_custom_message_entry(
            &app_message.custom_type,
            app_message.content.clone(),
            app_message.display,
            app_message.details.clone(),
        );
        self.refresh_finalized_context();
        let message = custom_message_to_agent(app_message);
        self.emit(AgentSessionEvent::MessageStart {
            message: message.clone(),
        });
        self.emit(AgentSessionEvent::MessageEnd { message });
    }

    /// Append custom messages queued while the agent was running.
    /// Called once the current turn's tool results are in agent state and session history.
    fn flush_pending_custom_messages(&self) {
        let pending = std::mem::take(&mut self.state.lock().pending_custom_messages);
        for app_message in &pending {
            self.append_custom_message(app_message);
        }
    }

    /// Send a user message to the agent. Always triggers a turn.
    /// When the agent is streaming, use deliverAs to specify how to queue the message.
    ///
    /// @param content User message content (string or content array)
    /// @param options.deliverAs Delivery mode when streaming: "steer" or "followUp"
    /// @param options.expandPromptTemplates Whether to dispatch extension commands and expand skill commands and prompt templates. Default: false.
    pub async fn send_user_message(
        &self,
        content: UserContent,
        options: Option<&AgentSessionSendUserMessageOptions>,
    ) -> anyhow::Result<()> {
        let (text, images) = match content {
            UserContent::Text(text) => (text, None),
            UserContent::Blocks(blocks) => {
                let mut text_parts = Vec::new();
                let mut images = Vec::new();
                for part in blocks {
                    match part {
                        UserContentBlock::Text(t) => text_parts.push(t.text),
                        UserContentBlock::Image(image) => images.push(image),
                    }
                }
                (
                    text_parts.join("\n"),
                    (!images.is_empty()).then_some(images),
                )
            }
        };
        self.prompt(
            &text,
            Some(&PromptOptions {
                expand_prompt_templates: Some(
                    options
                        .and_then(|o| o.expand_prompt_templates)
                        .unwrap_or(false),
                ),
                streaming_behavior: options.and_then(|o| o.deliver_as),
                images,
                source: Some(InputSource::Extension),
                preflight_result: None,
            }),
        )
        .await
    }

    /// Clear all queued messages and return them.
    /// Useful for restoring to editor when user aborts.
    /// @returns Object with steering and followUp arrays
    pub fn clear_queue(&self) -> AgentSessionClearQueueResult {
        let (steering, follow_up) = {
            let mut state = self.state.lock();
            (
                std::mem::take(&mut state.steering_messages),
                std::mem::take(&mut state.follow_up_messages),
            )
        };
        self.agent.clear_all_queues();
        self.emit_queue_update();
        AgentSessionClearQueueResult {
            steering,
            follow_up,
        }
    }

    /// Number of pending messages (includes both steering and follow-up)
    pub fn pending_message_count(&self) -> i64 {
        let state = self.state.lock();
        (state.steering_messages.len() + state.follow_up_messages.len()) as i64
    }

    /// Get pending steering messages (read-only)
    pub fn get_steering_messages(&self) -> Vec<String> {
        self.state.lock().steering_messages.clone()
    }

    /// Get pending follow-up messages (read-only)
    pub fn get_follow_up_messages(&self) -> Vec<String> {
        self.state.lock().follow_up_messages.clone()
    }

    pub fn resource_loader(&self) -> Arc<dyn ResourceLoader> {
        self.resource_loader.clone()
    }

    /// Abort current operation and wait for agent to become idle.
    pub async fn abort(&self) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock();
            if state.is_agent_run_active {
                state.agent_run_abort_requested = true;
            }
        }
        self.abort_retry();
        self.abort_compaction();
        self.abort_branch_summary();
        {
            let mut state = self.state.lock();
            if state.is_before_settle {
                state.abort_during_before_settle = true;
            }
        }
        self.agent.abort();
        self.wait_for_idle().await
    }

    pub async fn wait_for_idle(&self) -> anyhow::Result<()> {
        let mut rx = self.idle_tx.subscribe();
        loop {
            if self.is_idle() {
                return Ok(());
            }
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    // =========================================================================
    // Model Management
    // =========================================================================

    async fn emit_model_select(
        &self,
        next_model: &Model,
        previous_model: Option<Model>,
        source: ModelSelectSource,
    ) {
        if chat_models_are_equal(previous_model.as_ref(), Some(next_model)) {
            return;
        }
        let _ = self
            .runner()
            .emit(ExtensionEvent::ModelSelect(ModelSelectEvent {
                model: next_model.clone(),
                previous_model,
                source,
            }))
            .await;
    }

    /// Set model directly.
    /// Validates that auth is configured and saves to the session transcript.
    /// Persists to global defaults only when options.persist is true.
    /// @throws Error if no auth is configured for the model
    pub async fn set_model(
        &self,
        model: Model,
        options: Option<&ModelMutationOptions>,
    ) -> anyhow::Result<()> {
        if self
            .model_runtime
            .check_auth(&model.provider, None)
            .await?
            .is_none()
        {
            anyhow::bail!("No API key for {}/{}", model.provider, model.id);
        }
        let previous_model = self.model();
        let thinking_level = self.get_thinking_level_for_model_switch(Some(&model), None);
        self.agent.set_model(model.clone());
        self.session_manager
            .append_model_change(&model.provider, &model.id)?;
        if options.and_then(|o| o.persist) == Some(true) {
            self.settings_manager
                .set_default_model_and_provider(&model.provider, &model.id);
            self.add_persisted_default_to_non_empty_scope(&model);
        }
        // Per-model thinking level overrides take priority over the global default.
        self.set_thinking_level(thinking_level, None);
        self.emit_model_select(&model, previous_model, ModelSelectSource::Set)
            .await;
        Ok(())
    }

    fn add_persisted_default_to_non_empty_scope(&self, model: &Model) {
        {
            let mut state = self.state.lock();
            if state.scoped_models.is_empty() {
                return;
            }
            if state
                .scoped_models
                .iter()
                .any(|scoped| chat_models_are_equal(Some(&scoped.model), Some(model)))
            {
                return;
            }
            state.scoped_models.push(AgentSessionScopedModels {
                model: model.clone(),
                thinking_level: None,
            });
        }
        let Some(enabled_models) = self
            .settings_manager
            .get_enabled_models()
            .filter(|m| !m.is_empty())
        else {
            return;
        };
        let model_reference = format!("{}/{}", model.provider, model.id);
        if enabled_models
            .iter()
            .any(|pattern| pattern.to_lowercase() == model_reference.to_lowercase())
        {
            return;
        }
        let mut updated = enabled_models;
        updated.push(model_reference);
        self.settings_manager.set_enabled_models(Some(&updated));
    }

    /// Cycle to next/previous model.
    /// Uses scoped models (from --models flag) if available, otherwise all available models.
    /// @param direction - "forward" (default) or "backward"
    /// @returns The new model info, or undefined if only one model available
    pub async fn cycle_model(
        &self,
        direction: Option<AgentSessionCycleModelDirection>,
        options: Option<&ModelMutationOptions>,
    ) -> anyhow::Result<Option<ModelCycleResult>> {
        let direction = direction.unwrap_or(AgentSessionCycleModelDirection::Forward);
        let persist = options.and_then(|o| o.persist) == Some(true);
        let scoped = self.state.lock().scoped_models.clone();
        if !scoped.is_empty() {
            let available: HashSet<(String, String)> = self
                .model_runtime
                .get_available_snapshot()
                .into_iter()
                .map(|m| (m.provider, m.id))
                .collect();
            let scoped_models: Vec<AgentSessionScopedModels> = scoped
                .into_iter()
                .filter(|s| available.contains(&(s.model.provider.clone(), s.model.id.clone())))
                .collect();
            if scoped_models.len() <= 1 {
                return Ok(None);
            }
            let current_model = self.model();
            let current_index = scoped_models
                .iter()
                .position(|sm| chat_models_are_equal(Some(&sm.model), current_model.as_ref()))
                .unwrap_or(0);
            let len = scoped_models.len();
            let next_index = match direction {
                AgentSessionCycleModelDirection::Forward => (current_index + 1) % len,
                AgentSessionCycleModelDirection::Backward => (current_index + len - 1) % len,
            };
            let next = scoped_models[next_index].clone();
            let thinking_level =
                self.get_thinking_level_for_model_switch(Some(&next.model), next.thinking_level);
            self.agent.set_model(next.model.clone());
            self.session_manager
                .append_model_change(&next.model.provider, &next.model.id)?;
            if persist {
                self.settings_manager
                    .set_default_model_and_provider(&next.model.provider, &next.model.id);
                self.add_persisted_default_to_non_empty_scope(&next.model);
            }
            self.set_thinking_level(thinking_level, None);
            self.emit_model_select(&next.model, current_model, ModelSelectSource::Cycle)
                .await;
            return Ok(Some(ModelCycleResult {
                model: next.model,
                thinking_level: self.thinking_level(),
                is_scoped: true,
            }));
        }

        let available_models = self.model_runtime.get_available_snapshot();
        if available_models.len() <= 1 {
            return Ok(None);
        }
        let current_model = self.model();
        let current_index = available_models
            .iter()
            .position(|m| chat_models_are_equal(Some(m), current_model.as_ref()))
            .unwrap_or(0);
        let len = available_models.len();
        let next_index = match direction {
            AgentSessionCycleModelDirection::Forward => (current_index + 1) % len,
            AgentSessionCycleModelDirection::Backward => (current_index + len - 1) % len,
        };
        let next_model = available_models[next_index].clone();
        let thinking_level = self.get_thinking_level_for_model_switch(Some(&next_model), None);
        self.agent.set_model(next_model.clone());
        self.session_manager
            .append_model_change(&next_model.provider, &next_model.id)?;
        if persist {
            self.settings_manager
                .set_default_model_and_provider(&next_model.provider, &next_model.id);
            self.add_persisted_default_to_non_empty_scope(&next_model);
        }
        self.set_thinking_level(thinking_level, None);
        self.emit_model_select(&next_model, current_model, ModelSelectSource::Cycle)
            .await;
        Ok(Some(ModelCycleResult {
            model: next_model,
            thinking_level: self.thinking_level(),
            is_scoped: false,
        }))
    }

    // =========================================================================
    // Thinking Level Management
    // =========================================================================

    /// Set thinking level.
    /// Clamps to model capabilities based on available thinking levels.
    /// Saves the clamped level to the session transcript only if the level actually changes.
    /// Persists the requested level to global defaults only when options.persist is true.
    pub fn set_thinking_level(&self, level: ThinkingLevel, options: Option<&ModelMutationOptions>) {
        let available_levels = self.get_available_thinking_levels();
        let effective_level = if available_levels.contains(&level) {
            level
        } else {
            self.clamp_thinking_level(level)
        };
        let previous_level = self.agent.thinking_level();
        let is_changing = effective_level != previous_level;
        self.agent.set_thinking_level(effective_level);
        if options.and_then(|o| o.persist) == Some(true) {
            self.settings_manager.set_default_thinking_level(level);
        }
        if is_changing {
            let _ = self
                .session_manager
                .append_thinking_level_change(effective_level.as_str());
            self.emit(AgentSessionEvent::ThinkingLevelChanged {
                level: effective_level,
            });
            let runner = self.runner();
            spawn_detached(async move {
                let _ = runner
                    .emit(ExtensionEvent::ThinkingLevelSelect(ThinkingLevelSelectEvent {
                        level: effective_level,
                        previous_level,
                    }))
                    .await;
            });
        }
    }

    /// Cycle to next thinking level.
    /// @returns New level, or undefined if model doesn't support thinking
    pub fn cycle_thinking_level(
        &self,
        options: Option<&ModelMutationOptions>,
    ) -> Option<ThinkingLevel> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.get_available_thinking_levels();
        let current = self.thinking_level();
        let next_index = match levels.iter().position(|l| *l == current) {
            Some(i) => (i + 1) % levels.len(),
            None => 0,
        };
        let next_level = levels[next_index];
        self.set_thinking_level(next_level, options);
        Some(next_level)
    }

    /// Get available thinking levels for current model.
    /// The provider will clamp to what the specific model supports internally.
    pub fn get_available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            None => THINKING_LEVEL_OPTIONS.to_vec(),
            Some(model) => get_supported_thinking_levels(&model),
        }
    }

    /// Check if current model supports thinking/reasoning.
    pub fn supports_thinking(&self) -> bool {
        self.model().is_some_and(|m| m.reasoning)
    }

    fn get_thinking_level_for_model_switch(
        &self,
        target_model: Option<&Model>,
        explicit_level: Option<ThinkingLevel>,
    ) -> ThinkingLevel {
        if let Some(level) = explicit_level {
            return level;
        }
        // Per-model default takes priority when switching to a model that has one
        if let Some(target) = target_model
            && let Some(per_model) = self
                .settings_manager
                .get_model_thinking_level(&target.provider, &target.id)
        {
            return per_model;
        }
        self.settings_manager
            .get_default_thinking_level()
            .unwrap_or_else(|| self.thinking_level())
    }

    fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        match self.model() {
            Some(model) => clamp_thinking_level(&model, level),
            None => ThinkingLevel::Off,
        }
    }

    // =========================================================================
    // Queue Mode Management
    // =========================================================================

    fn sync_queue_modes_from_settings(&self) {
        self.agent.set_steering_mode(self.settings_manager.get_steering_mode());
        self.agent.set_follow_up_mode(self.settings_manager.get_follow_up_mode());
    }

    /// Set steering message mode.
    /// Saves to settings.
    pub fn set_steering_mode(&self, mode: AgentSessionSteeringMode) {
        self.agent.set_steering_mode(mode.clone());
        self.settings_manager.set_steering_mode(mode);
    }

    /// Set follow-up message mode.
    /// Saves to settings.
    pub fn set_follow_up_mode(&self, mode: AgentSessionSteeringMode) {
        self.agent.set_follow_up_mode(mode.clone());
        self.settings_manager.set_follow_up_mode(mode);
    }

    // =========================================================================
    // Compaction
    // =========================================================================

    /// Generate Pi's built-in compaction summary for manual and automatic compaction.
    async fn run_default_compaction(
        &self,
        preparation: &CompactionPreparation,
        model: &Model,
        custom_instructions: Option<&str>,
        signal: CancellationToken,
        reason: CompactionReason,
    ) -> anyhow::Result<CompactionResult> {
        // Resolve the request only when Pi summarizes itself: routing may call models or fail.
        let request = self
            .get_summarization_request_auth(model, Some(signal.clone()))
            .await?;
        let retry = retry_policy(&self.settings_manager);
        let callbacks = self.summarization_retry_callbacks(SummarizationRetrySource::Compaction {
            reason,
        });
        compact(
            preparation,
            &request.model,
            request.api_key.as_deref(),
            request.headers.as_ref(),
            custom_instructions,
            Some(signal),
            Some(request.thinking_level),
            self.agent.stream_fn(),
            request.env.as_ref(),
            Some(&retry),
            Some(&callbacks),
            None,
        )
        .await
    }

    fn clear_manual_compaction_state(&self) {
        self.state.lock().compaction_abort = None;
        self.resolve_idle_wait_if_idle();
    }

    /// Manually compact the session context.
    ///
    /// This is the manual entry point used by `/compact`, RPC, and extensions. It is
    /// separate from automatic threshold/overflow compaction. Aborts the current agent operation
    /// first. Manual compaction never retries or continues the interrupted agent turn.
    ///
    /// @param customInstructions Optional instructions for the compaction summary
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> anyhow::Result<CompactionResult> {
        self.abort().await?;
        let abort = CancellationToken::new();
        self.state.lock().compaction_abort = Some(abort.clone());
        self.emit(AgentSessionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });
        let mut from_extension = false;
        let mut cancelled_by_extension = false;

        let result: anyhow::Result<CompactionResult> = async {
            let Some(model) = self.model() else {
                anyhow::bail!("{}", format_no_model_selected_message());
            };
            let settings = self.compaction_settings(Some(&model))?;
            let path_entries = self.session_manager.get_branch(None);
            let Some(preparation) = prepare_compaction(&path_entries, &settings) else {
                // Check why we can't compact
                if matches!(path_entries.last(), Some(SessionEntry::Compaction(_))) {
                    anyhow::bail!("Already compacted");
                }
                anyhow::bail!("Nothing to compact (session too small)");
            };

            let runner = self.runner();
            let mut extension_compaction: Option<CompactionResult> = None;
            if runner.has_handlers("session_before_compact") {
                let result = runner
                    .emit(ExtensionEvent::SessionBeforeCompact(SessionBeforeCompactEvent {
                        preparation: preparation.clone(),
                        branch_entries: path_entries.clone(),
                        custom_instructions: custom_instructions.map(str::to_string),
                        reason: CompactionReason::Manual,
                        will_retry: false,
                        signal: abort.clone(),
                    }))
                    .await?;
                if let Some(SessionBeforeEventResult::SessionBeforeCompactResult(result)) = result {
                    if result.cancel == Some(true) {
                        cancelled_by_extension = true;
                        anyhow::bail!("Compaction cancelled");
                    }
                    if let Some(compaction) = result.compaction {
                        extension_compaction = Some(compaction);
                        from_extension = true;
                    }
                }
            }

            let computed = match extension_compaction {
                Some(compaction) => compaction,
                None => {
                    self.run_default_compaction(
                        &preparation,
                        &model,
                        custom_instructions,
                        abort.clone(),
                        CompactionReason::Manual,
                    )
                    .await?
                }
            };

            if abort.is_cancelled() {
                anyhow::bail!("Compaction cancelled");
            }

            self.finish_compaction(computed, from_extension, CompactionReason::Manual, false)
                .await
        }
        .await;

        match result {
            Ok(compaction_result) => {
                // compaction_end listeners may submit queued prompts, so expose idle state first.
                self.clear_manual_compaction_state();
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: Some(compaction_result.clone()),
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                Ok(compaction_result)
            }
            Err(error) => {
                let aborted = abort.is_cancelled() || cancelled_by_extension;
                let error_message = (!aborted).then(|| format!("Compaction failed: {error}"));
                self.clear_manual_compaction_state();
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Manual,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: error_message.clone(),
                });
                self.emit_session_compact_failed(SessionCompactFailedEvent {
                    reason: CompactionReason::Manual,
                    error_message,
                    aborted,
                    will_retry: false,
                    from_extension,
                })
                .await;
                Err(error)
            }
        }
    }

    /// Persist a compaction result, refresh context and notify extensions.
    async fn finish_compaction(
        &self,
        computed: CompactionResult,
        from_extension: bool,
        reason: CompactionReason,
        will_retry: bool,
    ) -> anyhow::Result<CompactionResult> {
        let CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            usage,
            details,
            ..
        } = computed;
        self.session_manager.append_compaction(
            &summary,
            Some(&first_kept_entry_id),
            tokens_before,
            details.clone(),
            Some(from_extension),
            usage.clone(),
        )?;
        let new_entries = self.session_manager.get_entries();
        self.refresh_finalized_context();
        let estimated_tokens_after =
            estimate_messages_tokens(&self.session_manager.build_session_projection().messages);

        // Get the saved compaction entry for the extension event
        let saved = new_entries.iter().find_map(|e| match e {
            SessionEntry::Compaction(c) if c.summary == summary => Some(c.clone()),
            _ => None,
        });
        if let Some(saved) = saved {
            let _ = self
                .runner()
                .emit(ExtensionEvent::SessionCompact(SessionCompactEvent {
                    compaction_entry: saved,
                    from_extension,
                    reason,
                    will_retry,
                }))
                .await;
        }
        Ok(CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            estimated_tokens_after: Some(estimated_tokens_after),
            usage,
            details,
        })
    }

    /// Cancel in-progress compaction (manual or auto).
    pub fn abort_compaction(&self) {
        let state = self.state.lock();
        if let Some(abort) = &state.compaction_abort {
            abort.cancel();
        }
        if let Some((_, abort)) = &state.auto_compaction_abort {
            abort.cancel();
        }
    }

    /// Cancel in-progress branch summarization.
    pub fn abort_branch_summary(&self) {
        if let Some(abort) = &self.state.lock().branch_summary_abort {
            abort.cancel();
        }
    }

    /// Dispatch automatic compaction after `agent_end` or before prompt submission.
    /// Returns whether the post-run loop should call `agent.continue()` for overflow recovery or
    /// queued messages.
    async fn check_compaction(
        &self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
        tool_results: &[AgentMessage],
    ) -> bool {
        let model = self.model();
        let Ok(settings) = self.compaction_settings(model.as_ref()) else {
            return false;
        };
        if !settings.enabled {
            return false;
        }
        // Skip if message was aborted (user cancelled) - unless skipAbortedCheck is false
        if skip_aborted_check && assistant_message.stop_reason == StopReason::Aborted {
            return false;
        }

        // Skip overflow check if the message came from a different model.
        let message_model = self.model_for_message(assistant_message);
        let same_model = message_model.is_some();
        let context_window = message_model
            .as_ref()
            .or(model.as_ref())
            .map(|m| m.context_window)
            .unwrap_or(0);

        // Skip compaction checks if this assistant message is older than the latest compaction.
        let branch = self.session_manager.get_branch(None);
        let compaction_entry = get_latest_compaction_entry(&branch);
        if let Some(compaction) = &compaction_entry
            && assistant_message.timestamp <= parse_timestamp_ms(&compaction.timestamp)
        {
            return false;
        }

        // Automatic cases 1 and 2: context overflow.
        let current_projection = self.session_manager.build_session_projection();
        let assistant_agent_message = AgentMessage::Assistant(assistant_message.clone());
        let assistant_entry_id = self.find_persisted_message_entry_id(&assistant_agent_message);
        let assistant_is_projected = match &assistant_entry_id {
            None => true,
            Some(id) => current_projection.entries.iter().any(|entry| {
                entry.source_entry.id() == id
                    && entry
                        .messages
                        .iter()
                        .any(|m| matches!(m, AgentMessage::Assistant(_)))
            }),
        };
        let assistant_index = assistant_entry_id
            .as_ref()
            .and_then(|id| branch.iter().position(|e| e.id() == id));
        let entries_after_assistant: &[SessionEntry] = match assistant_index {
            Some(i) => &branch[i + 1..],
            None => &[],
        };
        let has_post_assistant_context_edit = entries_after_assistant
            .iter()
            .any(|e| matches!(e, SessionEntry::ContextEdit(_)));
        let latest_assistant_edit = entries_after_assistant.iter().rev().find_map(|e| match e {
            SessionEntry::ContextEdit(edit)
                if Some(&edit.target_id) == assistant_entry_id.as_ref() =>
            {
                Some(edit.clone())
            }
            _ => None,
        });
        let assistant_retained_for_explicit_recovery = assistant_entry_id.is_none()
            || (!entries_after_assistant
                .iter()
                .any(|e| matches!(e, SessionEntry::Compaction(_)))
                && latest_assistant_edit
                    .as_ref()
                    .is_none_or(|edit| edit.replacement.is_some()));
        let assistant_usage_matches_projection =
            assistant_is_projected && !has_post_assistant_context_edit;
        let explicit_overflow = assistant_message.stop_reason == StopReason::Error
            && is_context_overflow(assistant_message, None);
        let context_overflow = same_model
            && ((explicit_overflow && assistant_retained_for_explicit_recovery)
                || (assistant_usage_matches_projection
                    && is_context_overflow(assistant_message, Some(context_window))));
        let recoverable_length = same_model
            && assistant_is_projected
            && is_recoverable_length(
                assistant_message,
                message_model.as_ref().map(|m| m.max_tokens).unwrap_or(0),
            );
        if context_overflow || recoverable_length {
            let will_retry = assistant_message.stop_reason != StopReason::Stop;

            // Case 2: the response completed successfully. Compact, but do not retry.
            if !will_retry {
                return self
                    .run_auto_compaction(CompactionReason::Overflow, false)
                    .await;
            }

            if self.state.lock().overflow_recovery_attempted {
                let error_message = if context_overflow {
                    "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                } else {
                    "Truncated response recovery failed after one compact-and-retry attempt."
                };
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(error_message.to_string()),
                });
                self.emit_session_compact_failed(SessionCompactFailedEvent {
                    reason: CompactionReason::Overflow,
                    error_message: Some(error_message.to_string()),
                    aborted: false,
                    will_retry: false,
                    from_extension: false,
                })
                .await;
                return false;
            }

            // Persistently omit the selected final attempt before post-run recovery compaction.
            self.state.lock().overflow_recovery_attempted = true;
            if let Err(error) = self.omit_recovery_attempt(assistant_message, tool_results) {
                tracing::warn!("{error}");
                return false;
            }
            let retry = self
                .run_auto_compaction(CompactionReason::Overflow, will_retry)
                .await;
            if retry {
                self.state.lock().failed_response = Some(assistant_message.clone());
            }
            return retry;
        }

        // Case 3: threshold compaction without retry.
        let projection = current_projection;
        let has_context_edits = projection
            .entries
            .iter()
            .any(|e| matches!(e.source_entry, SessionEntry::ContextEdit(_)));
        let direct_context_tokens = calculate_context_tokens(&assistant_message.usage);
        let context_tokens = if has_context_edits {
            estimate_projected_context_tokens(&projection, &branch).tokens
        } else if assistant_message.stop_reason == StopReason::Error || direct_context_tokens == 0 {
            let messages = self.agent.messages();
            let estimate = estimate_context_tokens(&messages);
            // Only usage-backed estimates need the stale pre-compaction check.
            if let Some(last_usage_index) = estimate.last_usage_index
                && let Some(compaction) = &compaction_entry
                && let Some(AgentMessage::Assistant(usage_message)) =
                    messages.get(last_usage_index as usize)
                && usage_message.timestamp <= parse_timestamp_ms(&compaction.timestamp)
            {
                return false;
            }
            estimate.tokens
        } else {
            direct_context_tokens
        };
        if should_compact(context_tokens, context_window, &settings) {
            return self
                .run_auto_compaction(CompactionReason::Threshold, false)
                .await;
        }
        false
    }

    /// Test hook: run the private compaction check (TS tests call `_checkCompaction`).
    #[doc(hidden)]
    pub async fn check_compaction_for_test(
        &self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> bool {
        self.check_compaction(assistant_message, skip_aborted_check, &[])
            .await
    }

    /// Test hook: run the private automatic compaction (TS tests call `_runAutoCompaction`).
    #[doc(hidden)]
    pub async fn run_auto_compaction_for_test(
        &self,
        reason: CompactionReason,
        will_retry: bool,
    ) -> bool {
        self.run_auto_compaction(reason, will_retry).await
    }

    /// Test hook: replace automatic compaction with `hook` (TS tests mock `_runAutoCompaction`).
    #[doc(hidden)]
    pub fn set_run_auto_compaction_override(&self, hook: Option<RunAutoCompactionOverride>) {
        *self.run_auto_compaction_override.lock() = hook;
    }

    /// Execute threshold or overflow compaction. Returns whether the post-run loop should call
    /// `agent.continue()`.
    async fn run_auto_compaction(&self, reason: CompactionReason, will_retry: bool) -> bool {
        let hook = self.run_auto_compaction_override.lock().clone();
        if let Some(hook) = hook {
            return hook(reason, will_retry);
        }
        let model = self.model();
        let mut started = false;
        let mut from_extension = false;
        let mut cancelled_by_extension = false;
        let mut abort: Option<(u64, CancellationToken)> = None;

        let result: anyhow::Result<bool> = async {
            let Some(model) = model.clone() else {
                return Ok(false);
            };
            let settings = self.compaction_settings(Some(&model))?;
            let path_entries = self.session_manager.get_branch(None);
            let Some(preparation) = prepare_compaction(&path_entries, &settings) else {
                return Ok(false);
            };

            let token = CancellationToken::new();
            let id = {
                let mut state = self.state.lock();
                let id = state.next_id;
                state.next_id += 1;
                state.auto_compaction_abort = Some((id, token.clone()));
                id
            };
            abort = Some((id, token.clone()));
            started = true;
            self.emit(AgentSessionEvent::CompactionStart { reason });
            if token.is_cancelled() {
                anyhow::bail!("This operation was aborted");
            }

            let runner = self.runner();
            let mut extension_compaction: Option<CompactionResult> = None;
            if runner.has_handlers("session_before_compact") {
                let result = runner
                    .emit(ExtensionEvent::SessionBeforeCompact(SessionBeforeCompactEvent {
                        preparation: preparation.clone(),
                        branch_entries: path_entries.clone(),
                        custom_instructions: None,
                        reason,
                        will_retry,
                        signal: token.clone(),
                    }))
                    .await?;
                if let Some(SessionBeforeEventResult::SessionBeforeCompactResult(result)) = result {
                    if result.cancel == Some(true) {
                        cancelled_by_extension = true;
                        anyhow::bail!("Compaction cancelled");
                    }
                    if let Some(compaction) = result.compaction {
                        extension_compaction = Some(compaction);
                        from_extension = true;
                    }
                }
            }
            if token.is_cancelled() {
                anyhow::bail!("This operation was aborted");
            }

            let computed = match extension_compaction {
                Some(compaction) => compaction,
                None => {
                    self.run_default_compaction(&preparation, &model, None, token.clone(), reason)
                        .await?
                }
            };
            if token.is_cancelled() {
                anyhow::bail!("This operation was aborted");
            }

            let result = self
                .finish_compaction(computed, from_extension, reason, will_retry)
                .await?;
            self.emit(AgentSessionEvent::CompactionEnd {
                reason,
                result: Some(result),
                aborted: false,
                will_retry,
                error_message: None,
            });

            if will_retry {
                return Ok(true);
            }
            // Auto-compaction can complete while follow-up/steering/custom messages are waiting.
            // Continue once so queued messages are delivered.
            Ok(self.agent.has_queued_messages())
        }
        .await;

        let outcome = match result {
            Ok(value) => value,
            Err(error) => {
                let aborted = abort.as_ref().is_some_and(|(_, t)| t.is_cancelled())
                    || cancelled_by_extension;
                if started {
                    let error_message = (!aborted).then(|| match reason {
                        CompactionReason::Overflow => {
                            format!("Context overflow recovery failed: {error}")
                        }
                        _ => format!("Auto-compaction failed: {error}"),
                    });
                    self.emit(AgentSessionEvent::CompactionEnd {
                        reason,
                        result: None,
                        aborted,
                        will_retry: false,
                        error_message: error_message.clone(),
                    });
                    self.emit_session_compact_failed(SessionCompactFailedEvent {
                        reason,
                        error_message,
                        aborted,
                        will_retry: false,
                        from_extension,
                    })
                    .await;
                }
                false
            }
        };
        if let Some((id, _)) = abort {
            let mut state = self.state.lock();
            if state.auto_compaction_abort.as_ref().map(|(i, _)| *i) == Some(id) {
                state.auto_compaction_abort = None;
            }
        }
        self.resolve_idle_wait_if_idle();
        outcome
    }

    /// Toggle auto-compaction setting.
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.settings_manager.set_compaction_enabled(enabled);
    }

    /// Whether auto-compaction is enabled
    pub fn auto_compaction_enabled(&self) -> bool {
        self.settings_manager.get_compaction_enabled()
    }

    pub async fn bind_extensions(&self, bindings: &ExtensionBindings) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock();
            if let Some(ui) = &bindings.ui_context {
                state.extension_ui_context = Some(ui.clone());
            }
            if let Some(mode) = bindings.mode {
                state.extension_mode = mode;
            }
            if let Some(actions) = &bindings.command_context_actions {
                state.extension_command_context_actions = Some(actions.clone());
            }
            if let Some(handler) = &bindings.abort_handler {
                state.extension_abort_handler = Some(handler.clone());
            }
            if let Some(handler) = &bindings.shutdown_handler {
                state.extension_shutdown_handler = Some(handler.clone());
            }
            if let Some(listener) = &bindings.on_error {
                state.extension_error_listener = Some(listener.clone());
            }
        }
        let runner = self.runner();
        self.apply_extension_bindings(&runner);
        runner
            .emit(ExtensionEvent::SessionStart(self.session_start_event.clone()))
            .await?;
        runner.report_unhandled_mcp_servers();
        self.extend_resources_from_extensions(
            if self.session_start_event.reason == SessionStartReason::Reload {
                ResourcesDiscoverReason::Reload
            } else {
                ResourcesDiscoverReason::Startup
            },
        )
        .await
    }

    async fn extend_resources_from_extensions(
        &self,
        reason: ResourcesDiscoverReason,
    ) -> anyhow::Result<()> {
        let runner = self.runner();
        if !runner.has_handlers("resources_discover") {
            return Ok(());
        }
        let discovered = runner.emit_resources_discover(&self.cwd, reason).await?;
        if discovered.skill_paths.is_empty()
            && discovered.prompt_paths.is_empty()
            && discovered.theme_paths.is_empty()
        {
            return Ok(());
        }
        let build = |entries: &[DiscoveredResourcePath]| -> Vec<(String, PathMetadata)> {
            entries
                .iter()
                .map(|entry| {
                    let source = Self::get_extension_source_label(&entry.extension_path);
                    let base_dir = if is_synthetic_path(&entry.extension_path) {
                        None
                    } else {
                        Path::new(&entry.extension_path)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                    };
                    (
                        entry.path.clone(),
                        PathMetadata {
                            source,
                            scope: SourceScope::Temporary,
                            origin: SourceOrigin::TopLevel,
                            base_dir,
                            package_root: None,
                        },
                    )
                })
                .collect()
        };
        let paths = ResourceExtensionPaths {
            skill_paths: Some(
                build(&discovered.skill_paths)
                    .into_iter()
                    .map(|(path, metadata)| ResourceExtensionPathsSkillPaths { path, metadata })
                    .collect(),
            ),
            prompt_paths: Some(
                build(&discovered.prompt_paths)
                    .into_iter()
                    .map(|(path, metadata)| ResourceExtensionPathsPromptPaths { path, metadata })
                    .collect(),
            ),
            theme_paths: Some(
                build(&discovered.theme_paths)
                    .into_iter()
                    .map(|(path, metadata)| ResourceExtensionPathsThemePaths { path, metadata })
                    .collect(),
            ),
        };
        self.resource_loader.extend_resources(&paths);
        let active = self.get_active_tool_names();
        self.rebuild_system_prompt(&active);
        Ok(())
    }

    fn get_extension_source_label(extension_path: &str) -> String {
        if is_synthetic_path(extension_path) {
            return format!(
                "extension:{}",
                extension_path.replace(['<', '>'], "")
            );
        }
        let base = Path::new(extension_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = base
            .strip_suffix(".ts")
            .or_else(|| base.strip_suffix(".js"))
            .unwrap_or(&base);
        format!("extension:{name}")
    }

    fn apply_extension_bindings(&self, runner: &Arc<ExtensionRunner>) {
        let (ui, mode, command_actions, listener, previous_unsubscriber) = {
            let mut state = self.state.lock();
            (
                state.extension_ui_context.clone(),
                state.extension_mode,
                state.extension_command_context_actions.clone(),
                state.extension_error_listener.clone(),
                state.extension_error_unsubscriber.take(),
            )
        };
        runner.set_ui_context(ui, Some(mode));
        runner.bind_command_context(command_actions);
        if let Some(unsubscribe) = previous_unsubscriber {
            unsubscribe();
        }
        let unsubscriber = listener.map(|listener| runner.on_error(listener));
        self.state.lock().extension_error_unsubscriber = unsubscriber;
    }

    fn refresh_current_model_from_registry(&self) {
        let Some(current) = self.model() else {
            return;
        };
        let Some(refreshed) = self.model_runtime.get_model(&current.provider, &current.id) else {
            return;
        };
        if refreshed == current {
            return;
        }
        self.agent.set_model(refreshed);
    }

    fn bind_extension_core(&self, runner: &Arc<ExtensionRunner>) {
        let actions = Arc::new(SessionExtensionActions {
            session: self.weak.clone(),
        });
        let context_actions = Arc::new(SessionContextActions {
            session: self.weak.clone(),
        });
        let weak = self.weak.clone();
        let provider_actions = ExtensionRunnerBindCoreProviderActions {
            register_provider: Some({
                let weak = weak.clone();
                Arc::new(move |name: String, config: ProviderConfig| {
                    let Some(session) = weak.upgrade() else {
                        return Ok(());
                    };
                    session
                        .model_runtime
                        .register_provider(&name, &provider_config_input(&config))?;
                    session.refresh_current_model_from_registry();
                    Ok(())
                })
            }),
            register_native_provider: Some({
                let weak = weak.clone();
                Arc::new(move |provider| {
                    let Some(session) = weak.upgrade() else {
                        return Ok(());
                    };
                    session.model_runtime.register_native_provider(provider)?;
                    session.refresh_current_model_from_registry();
                    Ok(())
                })
            }),
            unregister_provider: Some({
                let weak = weak.clone();
                Arc::new(move |name: String| {
                    if let Some(session) = weak.upgrade() {
                        session.model_runtime.unregister_provider(&name);
                        session.refresh_current_model_from_registry();
                    }
                })
            }),
            register_virtual_model: Some({
                let weak = weak.clone();
                Arc::new(move |definition: VirtualModelDefinition| {
                    let Some(session) = weak.upgrade() else {
                        return Ok(());
                    };
                    session.model_runtime.register_virtual_model(&definition)?;
                    session.refresh_current_model_from_registry();
                    Ok(())
                })
            }),
            unregister_virtual_model: Some({
                let weak = weak.clone();
                Arc::new(move |provider: String, id: String| {
                    if let Some(session) = weak.upgrade() {
                        session.model_runtime.unregister_virtual_model(&provider, &id);
                        session.refresh_current_model_from_registry();
                    }
                })
            }),
        };
        runner.bind_core(actions, context_actions, Some(&provider_actions));
    }

    fn get_commands(&self) -> Vec<SlashCommandInfo> {
        let runner = self.runner();
        let mut commands: Vec<SlashCommandInfo> = runner
            .get_registered_commands()
            .into_iter()
            .map(|command| SlashCommandInfo {
                name: command.invocation_name.clone(),
                description: command.description.clone(),
                source: SlashCommandSource::Extension,
                source_info: command.source_info.clone(),
            })
            .collect();
        commands.extend(self.prompt_templates().into_iter().map(|template| {
            SlashCommandInfo {
                name: template.name,
                description: Some(template.description),
                source: SlashCommandSource::Prompt,
                source_info: template.source_info,
            }
        }));
        commands.extend(self.resource_loader.get_skills().skills.into_iter().map(
            |skill| SlashCommandInfo {
                name: format!("skill:{}", skill.name),
                description: Some(skill.description),
                source: SlashCommandSource::Skill,
                source_info: skill.source_info,
            },
        ));
        commands
    }

    fn refresh_tool_registry(&self, active_tool_names: Option<Vec<String>>, include_all_extension_tools: bool) {
        // Tools that were already activated on registration.
        let previous_activated_on_registration: HashSet<String> = {
            let names: Vec<String> = self.state.lock().tool_registry.keys().cloned().collect();
            names
                .into_iter()
                .filter(|name| self.is_activated_on_registration(name))
                .collect()
        };
        let previous_active_tool_names = self.get_active_tool_names();
        let runner = self.runner();

        let registered_tools = runner.get_all_registered_tools();
        let mut all_custom_tools: Vec<RegisteredTool> = registered_tools;
        all_custom_tools.extend(self.custom_tools.iter().map(|definition| RegisteredTool {
            definition: definition.clone(),
            source_info: create_synthetic_source_info(
                &format!("<sdk:{}>", definition.name),
                &CreateSyntheticSourceInfoOptions {
                    source: "sdk".into(),
                    scope: None,
                    origin: None,
                    base_dir: None,
                },
            ),
        }));
        let all_custom_tools: Vec<RegisteredTool> = all_custom_tools
            .into_iter()
            .filter(|tool| self.is_allowed_tool(&tool.definition.name))
            .collect();
        let base_definitions = self.state.lock().base_tool_definitions.clone();
        let builtin_tools: Vec<RegisteredTool> = base_definitions
            .iter()
            .filter(|(name, _)| self.is_allowed_tool(name))
            .map(|(name, definition)| RegisteredTool {
                definition: definition.clone(),
                source_info: create_synthetic_source_info(
                    &format!("{BUILTIN_PATH_PREFIX}{name}"),
                    &CreateSyntheticSourceInfoOptions {
                        source: "builtin".into(),
                        scope: None,
                        origin: None,
                        base_dir: None,
                    },
                ),
            })
            .collect();
        let mut definition_registry: IndexMap<String, ToolDefinitionEntry> = builtin_tools
            .iter()
            .map(|tool| {
                (
                    tool.definition.name.clone(),
                    ToolDefinitionEntry {
                        definition: tool.definition.clone(),
                        source_info: tool.source_info.clone(),
                    },
                )
            })
            .collect();
        for tool in &all_custom_tools {
            definition_registry.insert(
                tool.definition.name.clone(),
                ToolDefinitionEntry {
                    definition: tool.definition.clone(),
                    source_info: tool.source_info.clone(),
                },
            );
        }
        let snippets: IndexMap<String, String> = definition_registry
            .values()
            .filter_map(|e| {
                Self::normalize_prompt_snippet(e.definition.prompt_snippet.as_deref())
                    .map(|s| (e.definition.name.clone(), s))
            })
            .collect();
        let guidelines: IndexMap<String, Vec<String>> = definition_registry
            .values()
            .filter_map(|e| {
                let g = Self::normalize_prompt_guidelines(e.definition.prompt_guidelines.as_deref());
                (!g.is_empty()).then(|| (e.definition.name.clone(), g))
            })
            .collect();
        {
            let mut state = self.state.lock();
            state.tool_definitions = definition_registry;
            state.tool_prompt_snippets = snippets;
            state.tool_prompt_guidelines = guidelines;
        }
        let wrapped_extension_tools = wrap_registered_tools(&all_custom_tools, runner.clone());
        let wrapped_builtin_tools = wrap_registered_tools(&builtin_tools, runner.clone());
        let mut tool_registry: IndexMap<String, AgentToolRef> = wrapped_builtin_tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();
        for tool in &wrapped_extension_tools {
            tool_registry.insert(tool.name().to_string(), tool.clone());
        }
        let registry_names: Vec<String> = tool_registry.keys().cloned().collect();
        self.state.lock().tool_registry = tool_registry;

        let explicit = active_tool_names.is_some();
        let mut next_active_tool_names: Vec<String> = active_tool_names
            .unwrap_or(previous_active_tool_names)
            .into_iter()
            .filter(|name| self.is_allowed_tool(name))
            .collect();

        if let Some(allowed) = &self.allowed_tools {
            for tool_name in &registry_names {
                // Naming or matching a tool activates it even when it is not active by default.
                if allowed(tool_name.clone()) && self.is_declarable(tool_name) {
                    next_active_tool_names.push(tool_name.clone());
                }
            }
        } else if include_all_extension_tools {
            for tool in &wrapped_extension_tools {
                if self.is_activated_on_registration(tool.name()) {
                    next_active_tool_names.push(tool.name().to_string());
                }
            }
        } else if !explicit {
            for tool_name in &registry_names {
                if !previous_activated_on_registration.contains(tool_name)
                    && self.is_activated_on_registration(tool_name)
                {
                    next_active_tool_names.push(tool_name.clone());
                }
            }
        }
        // Pending tools that are registered now become active.
        let pending: Vec<String> = self.state.lock().pending_tool_names.iter().cloned().collect();
        next_active_tool_names.extend(pending);
        let mut seen = HashSet::new();
        let unique: Vec<String> = next_active_tool_names
            .into_iter()
            .filter(|name| seen.insert(name.clone()))
            .collect();
        self.set_active_tools(&unique);
    }

    /// Whether activating the tool declares it to the model.
    fn is_declarable(&self, name: &str) -> bool {
        matches!(
            self.get_tool_exposure(name),
            ToolExposure::Direct | ToolExposure::ModelOnly
        )
    }

    /// Whether registering the tool activates it, which declares it to the model.
    fn is_activated_on_registration(&self, name: &str) -> bool {
        self.is_declarable(name)
            && self
                .state
                .lock()
                .tool_definitions
                .get(name)
                .and_then(|e| e.definition.default_active)
                != Some(false)
    }

    fn build_runtime(
        &self,
        active_tool_names: Option<Vec<String>>,
        flag_values: Option<IndexMap<String, FlagValue>>,
        include_all_extension_tools: bool,
    ) {
        let base_tool_definitions: IndexMap<String, ToolDefinition> = match &self.base_tools_override {
            Some(overrides) => overrides
                .iter()
                .map(|(name, tool)| {
                    (
                        name.clone(),
                        create_tool_definition_from_agent_tool(tool.clone()),
                    )
                })
                .collect(),
            None => {
                let auto_resize_images = self.settings_manager.get_image_auto_resize();
                let shell_command_prefix = self.settings_manager.get_shell_command_prefix();
                let shell_path = self.settings_manager.get_shell_path();
                create_all_tool_definitions(
                    &self.cwd,
                    Some(&ToolsOptions {
                        read: Some(ReadToolOptions {
                            auto_resize_images: Some(auto_resize_images),
                            resize_options: None,
                            operations: None,
                        }),
                        bash: Some(BashToolOptions {
                            operations: None,
                            command_prefix: shell_command_prefix,
                            shell_path,
                            expose_session_environment: None,
                            spawn_hook: None,
                        }),
                        powershell: None,
                        write: None,
                        edit: None,
                        grep: None,
                        find: None,
                        ls: None,
                    }),
                )
                .into_iter()
                .map(|(name, definition)| (name.as_str().to_string(), definition))
                .collect()
            }
        };
        self.state.lock().base_tool_definitions = base_tool_definitions;

        let extensions_result = self.resource_loader.get_extensions();
        if let Some(flag_values) = flag_values {
            let mut values = extensions_result.runtime.flag_values.lock();
            for (name, value) in flag_values {
                values.insert(name, value);
            }
        }

        let runner = ExtensionRunner::new(
            &extensions_result.extensions,
            extensions_result.runtime.clone(),
            &self.cwd,
            self.session_manager.clone(),
            Arc::new(ModelRegistry::new(self.model_runtime.clone())),
        );
        self.state.lock().extension_runner = Some(runner.clone());
        if let Some(runner_ref) = &self.extension_runner_ref {
            runner_ref.set(Some(runner.clone()));
        }
        self.bind_extension_core(&runner);
        self.apply_extension_bindings(&runner);

        let default_active_tool_names: Vec<String> = match &self.base_tools_override {
            Some(overrides) => overrides.keys().cloned().collect(),
            None => ["read", "bash", "edit", "write"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let base_active_tool_names = active_tool_names.unwrap_or(default_active_tool_names);
        self.refresh_tool_registry(Some(base_active_tool_names), include_all_extension_tools);
    }

    pub async fn reload(&self, options: Option<&AgentSessionReloadOptions>) -> anyhow::Result<()> {
        let old_runner = self.runner();
        let previous_flag_values = old_runner.get_flag_values();
        emit_session_shutdown_event(
            old_runner.clone(),
            &SessionShutdownEvent {
                reason: SessionShutdownReason::Reload,
                target_session_file: None,
            },
        )
        .await?;
        old_runner.invalidate(None);
        let get_default_tools = |session: &AgentSession| -> Vec<String> {
            if !session.uses_default_tools {
                return Vec::new();
            }
            let base = session.settings_manager.get_default_tools().unwrap_or_else(|| {
                DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect()
            });
            apply_tool_modifiers(&base, &session.default_tool_modifiers)
        };
        let previous_default_tools: HashSet<String> = get_default_tools(self).into_iter().collect();
        self.settings_manager.reload().await?;
        self.sync_queue_modes_from_settings();
        reset_api_providers();
        self.resource_loader.reload(None).await?;
        // Activate tools newly added to defaultTools. Removed ones stay active, and tools disabled
        // during the session stay disabled unless the setting newly adds them.
        let added_default_tools: Vec<String> = get_default_tools(self)
            .into_iter()
            .filter(|name| !previous_default_tools.contains(name))
            .collect();
        // Tools the new extensions register later, such as MCP tools, are pending until then.
        let active = self.get_active_tool_names();
        {
            let mut state = self.state.lock();
            for name in &active {
                state.pending_tool_names.insert(name.clone());
            }
        }
        let mut next_active = active;
        next_active.extend(added_default_tools);
        self.build_runtime(Some(next_active), Some(previous_flag_values), true);

        let has_bindings = {
            let state = self.state.lock();
            state.extension_ui_context.is_some()
                || state.extension_command_context_actions.is_some()
                || state.extension_shutdown_handler.is_some()
                || state.extension_error_listener.is_some()
        };
        if has_bindings {
            if let Some(before) = options.and_then(|o| o.before_session_start.clone()) {
                before().await?;
            }
            let runner = self.runner();
            runner
                .emit(ExtensionEvent::SessionStart(SessionStartEvent {
                    reason: SessionStartReason::Reload,
                    previous_session_file: None,
                }))
                .await?;
            runner.report_unhandled_mcp_servers();
            self.extend_resources_from_extensions(ResourcesDiscoverReason::Reload)
                .await?;
        }
        Ok(())
    }

    // =========================================================================
    // Auto-Retry
    // =========================================================================

    /// Check if an error is retryable (overloaded, rate limit, server errors).
    /// Context overflow errors are NOT retryable (handled by compaction instead).
    fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let context_window = self
            .model_for_message(message)
            .or_else(|| self.model())
            .map(|m| m.context_window)
            .unwrap_or(0);
        if is_context_overflow(message, Some(context_window)) {
            return false;
        }
        is_retryable_assistant_error(message)
    }

    /// Retry callbacks shared by compaction and branch-summary summarization calls.
    fn summarization_retry_callbacks(&self, source: SummarizationRetrySource) -> RetryCallbacks {
        let scheduled = self.weak.clone();
        let attempt_start = self.weak.clone();
        let finished = self.weak.clone();
        RetryCallbacks {
            on_retry_scheduled: Some(Box::new(
                move |attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String| {
                    if let Some(session) = scheduled.upgrade() {
                        session.emit(AgentSessionEvent::SummarizationRetryScheduled {
                            attempt: attempt as u64,
                            max_attempts: max_attempts as u64,
                            delay_ms,
                            error_message,
                        });
                    }
                    Box::pin(async {}) as BoxFuture<'static, ()>
                },
            )),
            on_retry_attempt_start: Some(Box::new(move || {
                if let Some(session) = attempt_start.upgrade() {
                    session.emit(AgentSessionEvent::SummarizationRetryAttemptStart(
                        source.clone(),
                    ));
                }
                Box::pin(async {}) as BoxFuture<'static, ()>
            })),
            on_retry_finished: Some(Box::new(
                move |_success: bool, _attempt: u32, _final_error: Option<String>| {
                    if let Some(session) = finished.upgrade() {
                        session.emit(AgentSessionEvent::SummarizationRetryFinished);
                    }
                    Box::pin(async {}) as BoxFuture<'static, ()>
                },
            )),
        }
    }

    fn finish_cancelled_retry(&self) {
        let attempt = {
            let mut state = self.state.lock();
            if state.retry_attempt == 0 {
                return;
            }
            std::mem::take(&mut state.retry_attempt)
        };
        self.emit(AgentSessionEvent::AutoRetryEnd {
            success: false,
            attempt,
            final_error: Some("Retry cancelled".to_string()),
        });
    }

    /// Prepare a retryable error for continuation with exponential backoff.
    /// @returns true if the caller should continue the agent, false otherwise
    async fn prepare_retry(&self, message: &AssistantMessage) -> bool {
        let settings = self.settings_manager.get_retry_settings();
        if !settings.enabled {
            return false;
        }
        let attempt = {
            let mut state = self.state.lock();
            state.retry_attempt += 1;
            if state.retry_attempt as i64 > settings.max_retries {
                // Preserve the completed attempt count so post-run handling can emit the final failure.
                state.retry_attempt -= 1;
                return false;
            }
            state.retry_attempt
        };
        let delay_ms = retry_delay_ms(
            settings.base_delay_ms.max(0) as u64,
            Some(settings.max_agent_delay_ms.max(0) as u64),
            attempt as u32,
        );
        self.emit(AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: settings.max_retries.max(0) as u64,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "Unknown error".to_string()),
        });

        // Keep the failed attempt in raw history while durably omitting it from model projection.
        if let Err(error) = self.omit_recovery_attempt(message, &[]) {
            tracing::warn!("{error}");
        }

        // Wait with exponential backoff (abortable)
        let abort = CancellationToken::new();
        self.state.lock().retry_abort = Some(abort.clone());
        let completed = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
            _ = abort.cancelled() => false,
        };
        self.state.lock().retry_abort = None;
        if !completed {
            // Aborted during sleep - emit end event so UI can clean up
            self.finish_cancelled_retry();
            return false;
        }
        true
    }

    /// Cancel in-progress retry.
    pub fn abort_retry(&self) {
        if let Some(abort) = &self.state.lock().retry_abort {
            abort.cancel();
        }
    }

    /// Whether auto-retry is currently in progress
    pub fn is_retrying(&self) -> bool {
        self.state.lock().retry_abort.is_some()
    }

    /// Whether auto-retry is enabled
    pub fn auto_retry_enabled(&self) -> bool {
        self.settings_manager.get_retry_enabled()
    }

    /// Toggle auto-retry setting.
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.settings_manager.set_retry_enabled(enabled);
    }

    // =========================================================================
    // Bash Execution
    // =========================================================================

    /// Execute a bash command.
    /// Adds result to agent context and session.
    /// @param command The bash command to execute
    /// @param onChunk Optional streaming callback for output
    /// @param options.excludeFromContext If true, command output won't be sent to LLM (!! prefix)
    /// @param options.id Optional identifier included in bash execution update events
    /// @param options.operations Custom BashOperations for remote execution
    pub async fn execute_bash(
        &self,
        command: &str,
        on_chunk: Option<Arc<dyn Fn(String) + Send + Sync>>,
        options: Option<&AgentSessionExecuteBashOptions>,
    ) -> anyhow::Result<BashResult> {
        let abort = CancellationToken::new();
        let id = {
            let mut state = self.state.lock();
            let id = state.next_id;
            state.next_id += 1;
            state.bash_aborts.push((id, abort.clone()));
            id
        };

        // Apply command prefix if configured (e.g., "shopt -s expand_aliases" for alias support)
        let prefix = self.settings_manager.get_shell_command_prefix();
        let shell_path = self.settings_manager.get_shell_path();
        let resolved_command = match prefix.filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        let operations = match options.and_then(|o| o.operations.clone()) {
            Some(operations) => operations,
            None => create_local_bash_operations(Some(&CreateLocalBashOperationsOptions {
                shell_path,
            })),
        };
        let weak = self.weak.clone();
        let event_id = options.and_then(|o| o.id.clone());
        let on_chunk_cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |delta: String| {
            if let Some(on_chunk) = &on_chunk {
                on_chunk(delta.clone());
            }
            if let Some(session) = weak.upgrade() {
                session.emit(AgentSessionEvent::BashExecutionUpdate {
                    id: event_id.clone(),
                    delta,
                });
            }
        });
        let result = execute_bash_with_operations(
            &resolved_command,
            &self.session_manager.get_cwd(),
            &operations,
            Some(&BashExecutorOptions {
                on_chunk: Some(on_chunk_cb),
                signal: Some(abort.clone()),
            }),
        )
        .await;
        self.state.lock().bash_aborts.retain(|(i, _)| *i != id);
        let result = result?;
        self.record_bash_result(
            command,
            &result,
            Some(&AgentSessionRecordBashResultOptions {
                exclude_from_context: options.and_then(|o| o.exclude_from_context),
            }),
        );
        Ok(result)
    }

    /// Record a bash execution result in session history.
    /// Used by executeBash and by extensions that handle bash execution themselves.
    pub fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        options: Option<&AgentSessionRecordBashResultOptions>,
    ) {
        let bash_message = BashExecutionMessage {
            role: BASH_EXECUTION_ROLE.to_string(),
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.clone(),
            timestamp: now_ms(),
            exclude_from_context: options.and_then(|o| o.exclude_from_context),
        };
        // If agent is streaming, defer adding to avoid breaking tool_use/tool_result ordering
        if self.is_streaming() {
            self.state.lock().pending_bash_messages.push(bash_message);
        } else {
            let _ = self
                .session_manager
                .append_message(AgentMessage::from(bash_message));
            self.refresh_finalized_context();
        }
    }

    /// Cancel running bash command.
    pub fn abort_bash(&self) {
        let aborts: Vec<CancellationToken> = self
            .state
            .lock()
            .bash_aborts
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        for abort in aborts {
            abort.cancel();
        }
    }

    /// Whether a bash command is currently running
    pub fn is_bash_running(&self) -> bool {
        !self.state.lock().bash_aborts.is_empty()
    }

    /// Whether there are pending bash messages waiting to be flushed
    pub fn has_pending_bash_messages(&self) -> bool {
        !self.state.lock().pending_bash_messages.is_empty()
    }

    /// Flush pending bash messages to agent state and session.
    fn flush_pending_bash_messages(&self) {
        let pending = std::mem::take(&mut self.state.lock().pending_bash_messages);
        if pending.is_empty() {
            return;
        }
        for message in pending {
            let _ = self
                .session_manager
                .append_message(AgentMessage::from(message));
        }
        self.refresh_finalized_context();
    }

    // =========================================================================
    // Session Management
    // =========================================================================

    /// Set a display name for the current session.
    pub fn set_session_name(&self, name: &str) {
        let _ = self.session_manager.append_session_info(name);
        let name = self.session_manager.get_session_name();
        self.emit(AgentSessionEvent::SessionInfoChanged { name: name.clone() });
        let runner = self.runner();
        spawn_detached(async move {
            let _ = runner
                .emit(ExtensionEvent::SessionInfoChanged(SessionInfoChangedEvent { name }))
                .await;
        });
    }

    // =========================================================================
    // Tree Navigation
    // =========================================================================

    /// Navigate to a different node in the session tree.
    /// Unlike fork() which creates a new session file, this stays in the same file.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: Option<&AgentSessionNavigateTreeOptions>,
    ) -> anyhow::Result<AgentSessionNavigateTreeResult> {
        let options = options.cloned().unwrap_or_default();
        if self.is_streaming() {
            anyhow::bail!(
                "Wait for the current response to finish before navigating the session tree."
            );
        }
        if self.is_compacting() {
            anyhow::bail!(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree."
            );
        }
        let old_leaf_id = self.session_manager.get_leaf_id();
        // No-op if already at target
        if old_leaf_id.as_deref() == Some(target_id) {
            return Ok(AgentSessionNavigateTreeResult {
                editor_text: None,
                cancelled: false,
                aborted: None,
                summary_entry: None,
            });
        }
        let summarize = options.summarize == Some(true);
        // Model required for summarization
        if summarize && self.model().is_none() {
            anyhow::bail!("No model available for summarization");
        }
        let Some(target_entry) = self.session_manager.get_entry(target_id) else {
            anyhow::bail!("Entry {target_id} not found");
        };

        // Collect entries to summarize (from old leaf to common ancestor)
        let collected = collect_entries_for_branch_summary(
            &self.session_manager,
            old_leaf_id.as_deref(),
            target_id,
        );
        let mut custom_instructions = options.custom_instructions.clone();
        let mut replace_instructions = options.replace_instructions;
        let mut label = options.label.clone();
        let preparation = TreePreparation {
            target_id: target_id.to_string(),
            old_leaf_id: old_leaf_id.clone(),
            common_ancestor_id: collected.common_ancestor_id.clone(),
            entries_to_summarize: collected.entries.clone(),
            user_wants_summary: summarize,
            custom_instructions: custom_instructions.clone(),
            replace_instructions,
            label: label.clone(),
        };

        // Set up abort controller for summarization
        let abort = CancellationToken::new();
        self.state.lock().branch_summary_abort = Some(abort.clone());

        let result: anyhow::Result<AgentSessionNavigateTreeResult> = async {
            let runner = self.runner();
            let mut extension_summary: Option<SessionBeforeTreeSummary> = None;
            let mut from_extension = false;

            // Emit session_before_tree event
            if runner.has_handlers("session_before_tree") {
                let result = runner
                    .emit(ExtensionEvent::SessionBeforeTree(SessionBeforeTreeEvent {
                        preparation: preparation.clone(),
                        signal: abort.clone(),
                    }))
                    .await?;
                if let Some(SessionBeforeEventResult::SessionBeforeTreeResult(result)) = result {
                    if result.cancel == Some(true) {
                        return Ok(AgentSessionNavigateTreeResult {
                            editor_text: None,
                            cancelled: true,
                            aborted: None,
                            summary_entry: None,
                        });
                    }
                    if let Some(summary) = result.summary
                        && summarize
                    {
                        extension_summary = Some(summary);
                        from_extension = true;
                    }
                    // Allow extensions to override instructions and label
                    if result.custom_instructions.is_some() {
                        custom_instructions = result.custom_instructions;
                    }
                    if result.replace_instructions.is_some() {
                        replace_instructions = result.replace_instructions;
                    }
                    if result.label.is_some() {
                        label = result.label;
                    }
                }
            }

            // Run default summarizer if needed
            let mut summary_text: Option<String> = None;
            let mut summary_details: Option<Value> = None;
            let mut summary_usage: Option<Usage> = None;
            if summarize && !collected.entries.is_empty() && extension_summary.is_none() {
                let model = self.model().ok_or_else(|| err("No model available for summarization"))?;
                let request = self
                    .get_summarization_request_auth(&model, Some(abort.clone()))
                    .await?;
                let branch_summary_settings = self.settings_manager.get_branch_summary_settings();
                let result = generate_branch_summary(
                    &collected.entries,
                    &GenerateBranchSummaryOptions {
                        model: request.model,
                        api_key: request.api_key,
                        headers: request.headers,
                        env: request.env,
                        signal: abort.clone(),
                        custom_instructions: custom_instructions.clone(),
                        replace_instructions,
                        reserve_tokens: Some(branch_summary_settings.reserve_tokens),
                        stream_fn: self.agent.stream_fn(),
                        retry: Some(retry_policy(&self.settings_manager)),
                        callbacks: Some(self.summarization_retry_callbacks(
                            SummarizationRetrySource::BranchSummary,
                        )),
                    },
                )
                .await?;
                if result.aborted == Some(true) {
                    return Ok(AgentSessionNavigateTreeResult {
                        editor_text: None,
                        cancelled: true,
                        aborted: Some(true),
                        summary_entry: None,
                    });
                }
                if let Some(error) = result.error {
                    anyhow::bail!("{error}");
                }
                summary_text = result.summary;
                summary_usage = result.usage;
                summary_details = Some(serde_json::json!({
                    "readFiles": result.read_files.unwrap_or_default(),
                    "modifiedFiles": result.modified_files.unwrap_or_default(),
                }));
            } else if let Some(summary) = extension_summary {
                summary_text = Some(summary.summary);
                summary_details = summary.details;
                summary_usage = summary.usage;
            }

            // Determine the new leaf position based on target type
            let (new_leaf_id, editor_text) = match &target_entry {
                SessionEntry::Message(e) if matches!(e.message, AgentMessage::User(_)) => {
                    // User message: leaf = parent (null if root), text goes to editor
                    (e.parent_id.clone(), Some(agent_message_text(&e.message)))
                }
                SessionEntry::CustomMessage(e) => {
                    // Custom message: leaf = parent (null if root), text goes to editor
                    (e.parent_id.clone(), Some(user_content_text(&e.content)))
                }
                // Non-user message: leaf = selected node
                _ => (Some(target_id.to_string()), None),
            };

            // Switch leaf (with or without summary)
            let summary_text = summary_text.filter(|s| !s.is_empty());
            let mut summary_entry: Option<BranchSummaryEntry> = None;
            if let Some(summary) = &summary_text {
                let summary_id = self.session_manager.branch_with_summary(
                    new_leaf_id.as_deref(),
                    summary,
                    summary_details.clone(),
                    Some(from_extension),
                    summary_usage.clone(),
                )?;
                summary_entry = match self.session_manager.get_entry(&summary_id) {
                    Some(SessionEntry::BranchSummary(entry)) => Some(entry),
                    _ => None,
                };
                // Attach label to the summary entry
                if let Some(label) = label.as_deref().filter(|l| !l.is_empty()) {
                    self.session_manager
                        .append_label_change(&summary_id, Some(label))?;
                }
            } else if let Some(leaf) = &new_leaf_id {
                // No summary, navigating to non-root
                self.session_manager.branch(leaf)?;
            } else {
                // No summary, navigating to root - reset leaf
                self.session_manager.reset_leaf();
            }

            // Attach label to target entry when not summarizing (no summary entry to label)
            if let Some(label) = label.as_deref().filter(|l| !l.is_empty())
                && summary_text.is_none()
            {
                self.session_manager
                    .append_label_change(target_id, Some(label))?;
            }

            // Update finalized context from the canonical session projection.
            self.refresh_finalized_context();
            self.restore_tools_from_transcript(&[]);

            let _ = runner
                .emit(ExtensionEvent::SessionTree(SessionTreeEvent {
                    new_leaf_id: self.session_manager.get_leaf_id(),
                    old_leaf_id: old_leaf_id.clone(),
                    summary_entry: summary_entry.clone(),
                    from_extension: summary_text.is_some().then_some(from_extension),
                }))
                .await;

            Ok(AgentSessionNavigateTreeResult {
                editor_text,
                cancelled: false,
                aborted: None,
                summary_entry,
            })
        }
        .await;

        self.state.lock().branch_summary_abort = None;
        self.resolve_idle_wait_if_idle();
        result
    }

    /// Get all user messages from session for fork selector.
    pub fn get_user_messages_for_forking(
        &self,
    ) -> Vec<AgentSessionGetUserMessagesForForkingResult> {
        self.session_manager
            .get_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(e) => match &e.message {
                    AgentMessage::User(user) => {
                        let text = user_content_text(&user.content);
                        (!text.is_empty()).then(|| AgentSessionGetUserMessagesForForkingResult {
                            entry_id: e.id.clone(),
                            text,
                        })
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Get session statistics. Aggregates over ALL session entries (including
    /// history that was compacted away), so token/cost totals reflect what was
    /// actually billed across the session.
    pub fn get_session_stats(&self) -> SessionStats {
        let mut user_messages = 0i64;
        let mut assistant_messages = 0i64;
        let mut tool_results = 0f64;
        let mut total_messages = 0i64;
        let mut tool_calls = 0f64;
        let mut totals = create_usage_totals();

        for entry in self.session_manager.get_entries() {
            match &entry {
                SessionEntry::Usage(e) => add_usage_to_totals(&mut totals, &e.usage),
                SessionEntry::BranchSummary(e) => {
                    if let Some(usage) = &e.usage {
                        add_usage_to_totals(&mut totals, usage);
                    }
                }
                SessionEntry::Compaction(e) => {
                    if let Some(usage) = &e.usage {
                        add_usage_to_totals(&mut totals, usage);
                    }
                }
                _ => {}
            }
            let SessionEntry::Message(e) = &entry else {
                continue;
            };
            total_messages += 1;
            match &e.message {
                AgentMessage::User(_) => user_messages += 1,
                AgentMessage::ToolResult(result) => {
                    tool_results += 1.0;
                    if let Some(usage) = &result.usage {
                        add_usage_to_totals(&mut totals, usage);
                    }
                }
                AgentMessage::Assistant(assistant) => {
                    assistant_messages += 1;
                    tool_calls += assistant
                        .content
                        .iter()
                        .filter(|c| matches!(c, AssistantContent::ToolCall(_)))
                        .count() as f64;
                    add_usage_to_totals(&mut totals, &assistant.usage);
                }
                _ => {}
            }
        }

        SessionStats {
            session_file: self.session_file(),
            session_id: self.session_id(),
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens: SessionStatsTokens {
                input: totals.input,
                output: totals.output,
                cache_read: totals.cache_read,
                cache_write: totals.cache_write,
                total: (totals.input + totals.output + totals.cache_read + totals.cache_write)
                    as i64,
            },
            cost: totals.cost,
            context_usage: self.get_context_usage(),
        }
    }

    pub fn get_context_usage(&self) -> Option<ContextUsage> {
        let model = self.limits_model()?;
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }

        // After compaction, the last assistant usage reflects pre-compaction context size.
        let projection = self.session_manager.build_session_projection();
        let branch = self.session_manager.get_branch(None);
        if let Some(latest_compaction) = get_latest_compaction_entry(&branch) {
            let projected_assistants: HashSet<String> = projection
                .entries
                .iter()
                .filter(|entry| {
                    entry.messages.iter().any(|message| match message {
                        AgentMessage::Assistant(m) => {
                            m.stop_reason != StopReason::Aborted
                                && m.stop_reason != StopReason::Error
                                && calculate_context_tokens(&m.usage) > 0
                        }
                        _ => false,
                    })
                })
                .map(|entry| entry.source_entry.id().to_string())
                .collect();
            let compaction_index = branch
                .iter()
                .position(|e| e.id() == latest_compaction.id)
                .map(|i| i as isize)
                .unwrap_or(-1);
            let has_post_compaction_usage = branch
                .iter()
                .skip((compaction_index + 1) as usize)
                .any(|e| projected_assistants.contains(e.id()));
            if !has_post_compaction_usage {
                return Some(ContextUsage {
                    tokens: None,
                    context_window,
                    percent: None,
                });
            }
        }

        let estimate = estimate_projected_context_tokens(&projection, &branch);
        let percent = (estimate.tokens as f64 / context_window as f64) * 100.0;
        Some(ContextUsage {
            tokens: Some(estimate.tokens),
            context_window,
            percent: Some(percent),
        })
    }

    /// Export session to HTML.
    /// @param outputPath Optional output path (defaults to session directory)
    /// @param options Optional export presentation settings
    /// @returns Path to exported file
    pub async fn export_to_html(
        &self,
        output_path: Option<&str>,
        options: Option<&AgentSessionExportToHtmlOptions>,
    ) -> anyhow::Result<String> {
        let theme_name = [
            options.and_then(|o| o.theme_name.clone()),
            self.settings_manager.get_theme(),
        ]
        .into_iter()
        .flatten()
        .find(|candidate| get_theme_by_name(candidate).is_some());

        // Create tool renderer (for custom tool HTML rendering)
        let weak = self.weak.clone();
        let tool_renderer = create_tool_html_renderer(&ToolHtmlRendererDeps {
            get_tool_renderers: Arc::new(move |name: String| {
                let session = weak.upgrade()?;
                let runner = session.runner();
                let base_session = session.clone();
                let base_name = name.clone();
                runner.resolve_tool_renderers(
                    &name,
                    Arc::new(move || {
                        base_session
                            .get_tool_definition(&base_name)
                            .map(|d| d.renderers())
                    }),
                )
            }),
            theme: theme(),
            cwd: self.session_manager.get_cwd(),
            width: None,
        });
        let state = self.state();
        export_session_to_html(
            &self.session_manager,
            Some(&state),
            Some(ExportSessionToHtmlOptions::ExportOptions(ExportOptions {
                output_path: output_path.map(str::to_string),
                theme_name,
                tool_renderer: Some(tool_renderer),
            })),
        )
        .await
    }

    /// Export the current session branch to a JSONL file.
    /// Writes the session header followed by all entries on the current branch path.
    /// @param outputPath Target file path. If omitted, generates a timestamped file in cwd.
    /// @returns The resolved output file path.
    pub fn export_to_jsonl(&self, output_path: Option<&str>) -> anyhow::Result<String> {
        export_session_to_jsonl(&self.session_manager, output_path, None)
    }

    /// Ask the current model to describe what went wrong in this session for a bug report.
    /// Used when the user declines to share the transcript itself.
    pub async fn summarize_for_bug_report(
        &self,
        options: &AgentSessionSummarizeForBugReportOptions,
    ) -> anyhow::Result<String> {
        let Some(model) = self.model() else {
            anyhow::bail!("No model selected");
        };
        let request = self
            .get_summarization_request_auth(&model, Some(options.signal.clone()))
            .await?;
        generate_bug_report_summary(&GenerateBugReportSummaryOptions {
            messages: self.messages(),
            hint: options.hint.clone(),
            model: request.model,
            api_key: request.api_key,
            headers: request.headers,
            env: request.env,
            signal: options.signal.clone(),
            thinking_level: Some(request.thinking_level),
            stream_fn: self.agent.stream_fn(),
            retry: Some(retry_policy(&self.settings_manager)),
            session_id: Some(self.session_id()),
        })
        .await
    }

    // =========================================================================
    // Utilities
    // =========================================================================

    /// Get text content of last assistant message.
    /// Useful for /copy command.
    /// @returns Text content, or undefined if no assistant message exists
    pub fn get_last_assistant_text(&self) -> Option<String> {
        let messages = self.messages();
        let last_assistant = messages.iter().rev().find_map(|m| match m {
            AgentMessage::Assistant(a) => {
                // Skip aborted messages with no content
                if a.stop_reason == StopReason::Aborted && a.content.is_empty() {
                    None
                } else {
                    Some(a)
                }
            }
            _ => None,
        })?;
        let mut text = String::new();
        for content in &last_assistant.content {
            if let AssistantContent::Text(t) = content {
                text.push_str(&t.text);
            }
        }
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    // =========================================================================
    // Extension System
    // =========================================================================

    pub fn create_replaced_session_context(&self) -> ReplacedSessionContext {
        ReplacedSessionContext {
            cmd: self.runner().create_command_context(),
            messaging: Arc::new(SessionReplacedActions {
                session: self.weak.clone(),
            }),
        }
    }

    /// Check if extensions have handlers for a specific event type.
    pub fn has_extension_handlers(&self, event_type: &str) -> bool {
        self.runner().has_handlers(event_type)
    }

    /// Get the extension runner (for setting UI context and error handlers).
    pub fn extension_runner(&self) -> Arc<ExtensionRunner> {
        self.runner()
    }
}

/// Which boundary a context preview is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryKind {
    TurnEnd,
    AgentBeforeSettle,
}

fn message_role(message: &Message) -> &'static str {
    match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

/// Untyped extension handlers can return messages with null/missing content.
fn normalize_replacement_content(message: AgentMessage) -> AgentMessage {
    match message {
        AgentMessage::Custom(mut custom) if custom.role == CUSTOM_ROLE => {
            if custom.fields.get("content").is_none_or(Value::is_null) {
                custom
                    .fields
                    .insert("content".into(), Value::Array(Vec::new()));
            }
            AgentMessage::Custom(custom)
        }
        other => other,
    }
}

/// An agent tool whose model-facing description a `prepareLoadout` hook replaced.
struct DescribedTool {
    inner: AgentToolRef,
    description: String,
}

#[async_trait::async_trait]
impl pi_agent::types::AgentTool for DescribedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &Value {
        self.inner.parameters()
    }
    fn parameters_typebox(&self) -> bool {
        self.inner.parameters_typebox()
    }
    fn constrained_sampling(&self) -> Option<&pi_ai::types::ConstrainedSampling> {
        self.inner.constrained_sampling()
    }
    fn prepare_arguments(&self, args: &pi_ai::types::JsonObject) -> Option<pi_ai::types::JsonObject> {
        self.inner.prepare_arguments(args)
    }
    fn output_schema(&self) -> Option<&Value> {
        self.inner.output_schema()
    }
    fn replay(&self) -> Option<pi_agent::types::ToolReplay> {
        self.inner.replay()
    }
    fn execution_mode(&self) -> Option<pi_agent::types::ToolExecutionMode> {
        self.inner.execution_mode()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: pi_agent::types::AgentToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.inner
            .execute(tool_call_id, params, signal, on_update)
            .await
    }
}

/// The session's tools as `prepareLoadout` hooks see them.
struct SessionToolLoadout {
    declared: Vec<AgentToolRef>,
    callable: Vec<AgentToolRef>,
    registered: Vec<AgentToolRef>,
    definitions: IndexMap<String, ToolDefinitionEntry>,
    guidelines: IndexMap<String, Vec<String>>,
}

impl ToolLoadout for SessionToolLoadout {
    fn declared(&self) -> Vec<AgentToolRef> {
        self.declared.clone()
    }
    fn callable(&self) -> Vec<AgentToolRef> {
        self.callable.clone()
    }
    fn registered(&self) -> Vec<AgentToolRef> {
        self.registered.clone()
    }
    fn get_exposure(&self, name: &str) -> ToolExposure {
        self.definitions
            .get(name)
            .map(|e| e.definition.effective_exposure())
            .unwrap_or(ToolExposure::Direct)
    }
    fn get_namespace(&self, name: &str) -> Option<ToolNamespace> {
        self.definitions
            .get(name)
            .and_then(|e| e.definition.namespace.clone())
    }
    fn get_prompt_guidelines(&self, name: &str) -> Vec<String> {
        self.guidelines.get(name).cloned().unwrap_or_default()
    }
}

/// Backend of nested tool calls (`ctx.executeTool()`).
struct SessionNestedHost {
    session: Weak<AgentSession>,
}

#[async_trait::async_trait]
impl NestedToolCallHost for SessionNestedHost {
    fn get_tools(&self) -> Vec<AgentToolRef> {
        self.session
            .upgrade()
            .map(|s| s.get_callable_tools(None))
            .unwrap_or_default()
    }

    fn is_sequential(&self) -> bool {
        self.session
            .upgrade()
            .is_some_and(|s| s.agent.tool_execution() == ToolExecutionMode::Sequential)
    }

    async fn run_tool_call(
        &self,
        tool_call: AgentToolCall,
        parent_tool_call_id: &str,
        signal: Option<CancellationToken>,
        on_update: NestedUpdateSink,
    ) -> AgentToolCallOutcome {
        let failure = |tool_call: AgentToolCall, text: &str| AgentToolCallOutcome {
            tool_call,
            result: AgentToolResult {
                content: vec![UserContentBlock::text(text)],
                details: Some(Value::Object(serde_json::Map::new())),
                ..Default::default()
            },
            is_error: true,
            duration_ms: None,
        };
        let Some(session) = self.session.upgrade() else {
            return failure(tool_call, "Session is disposed");
        };
        let Some(assistant_message) = session.find_last_assistant_message() else {
            return failure(tool_call, "No assistant message issued this call");
        };
        let before_session = session.weak.clone();
        let before_parent = parent_tool_call_id.to_string();
        let after_session = session.weak.clone();
        let after_parent = parent_tool_call_id.to_string();
        let state = session.agent.state();
        let mut options = RunToolCallOptions::new(
            session.get_callable_tools(None),
            assistant_message,
            AgentContext {
                messages: state.messages,
                tools: Some(state.tools),
            },
        );
        options.before_tool_call = Some(Arc::new(move |context, _signal| {
            let session = before_session.clone();
            let parent = before_parent.clone();
            Box::pin(async move {
                match session.upgrade() {
                    Some(session) => session.before_tool_call(context, Some(parent)).await,
                    None => Ok(None),
                }
            })
        }));
        options.after_tool_call = Some(Arc::new(move |context, _signal| {
            let session = after_session.clone();
            let parent = after_parent.clone();
            Box::pin(async move {
                match session.upgrade() {
                    Some(session) => session.after_tool_call(context, Some(parent)).await,
                    None => Ok(None),
                }
            })
        }));
        options.signal = signal;
        options.on_update = Some(on_update);
        run_tool_call(tool_call, options).await
    }

    async fn emit(&self, event: NestedToolExecutionEvent) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let runner = session.runner();
        let (extension_event, session_event) = match event {
            NestedToolExecutionEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
                parent_tool_call_id,
            } => (
                ExtensionEvent::ToolExecutionStart(ToolExecutionStartEvent {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    parent_tool_call_id: Some(parent_tool_call_id.clone()),
                }),
                AgentSessionEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                    parent_tool_call_id: Some(parent_tool_call_id),
                },
            ),
            NestedToolExecutionEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
                parent_tool_call_id,
            } => (
                ExtensionEvent::ToolExecutionUpdate(ToolExecutionUpdateEvent {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    partial_result: serde_json::to_value(&partial_result).unwrap_or(Value::Null),
                    parent_tool_call_id: Some(parent_tool_call_id.clone()),
                }),
                AgentSessionEvent::ToolExecutionUpdate {
                    tool_call_id,
                    tool_name,
                    args,
                    partial_result,
                    parent_tool_call_id: Some(parent_tool_call_id),
                },
            ),
            NestedToolExecutionEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                duration_ms,
                parent_tool_call_id,
            } => (
                ExtensionEvent::ToolExecutionEnd(ToolExecutionEndEvent {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result: serde_json::to_value(&result).unwrap_or(Value::Null),
                    is_error,
                    duration_ms,
                    parent_tool_call_id: Some(parent_tool_call_id.clone()),
                }),
                AgentSessionEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    result,
                    is_error,
                    duration_ms,
                    parent_tool_call_id: Some(parent_tool_call_id),
                },
            ),
        };
        let _ = runner.emit(extension_event).await;
        session.emit(session_event);
    }
}

/// `pi.*` actions backed by the session (`bindCore()` actions).
struct SessionExtensionActions {
    session: Weak<AgentSession>,
}

impl SessionExtensionActions {
    fn session(&self) -> anyhow::Result<Arc<AgentSession>> {
        self.session
            .upgrade()
            .ok_or_else(|| err("The session for this extension was disposed"))
    }
}

#[async_trait::async_trait]
impl ExtensionActions for SessionExtensionActions {
    fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()> {
        let session = self.session()?;
        spawn_detached(async move {
            let result = session
                .send_custom_message(
                    &AgentSessionSendCustomMessageMessage {
                        custom_type: message.custom_type,
                        content: message.content,
                        display: message.display,
                        details: message.details,
                    },
                    Some(&AgentSessionSendCustomMessageOptions {
                        trigger_turn: options.trigger_turn,
                        deliver_as: options.deliver_as.map(|d| match d {
                            DeliverAs::Steer => AgentSessionSendCustomMessageOptionsDeliverAs::Steer,
                            DeliverAs::FollowUp => {
                                AgentSessionSendCustomMessageOptionsDeliverAs::FollowUp
                            }
                            DeliverAs::NextTurn => {
                                AgentSessionSendCustomMessageOptionsDeliverAs::NextTurn
                            }
                        }),
                    }),
                )
                .await;
            if let Err(error) = result {
                session.runner().emit_error(&ExtensionError {
                    extension_path: "<runtime>".into(),
                    event: "send_message".into(),
                    error: error.to_string(),
                    stack: None,
                });
            }
        });
        Ok(())
    }

    fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()> {
        let session = self.session()?;
        spawn_detached(async move {
            let result = session
                .send_user_message(
                    content,
                    Some(&AgentSessionSendUserMessageOptions {
                        deliver_as: options.deliver_as.map(|d| match d {
                            StreamingBehavior::Steer => PromptOptionsStreamingBehavior::Steer,
                            StreamingBehavior::FollowUp => PromptOptionsStreamingBehavior::FollowUp,
                        }),
                        expand_prompt_templates: options.expand_prompt_templates,
                    }),
                )
                .await;
            if let Err(error) = result {
                session.runner().emit_error(&ExtensionError {
                    extension_path: "<runtime>".into(),
                    event: "send_user_message".into(),
                    error: error.to_string(),
                    stack: None,
                });
            }
        });
        Ok(())
    }

    fn append_entry(&self, custom_type: &str, data: Option<Value>) -> anyhow::Result<()> {
        let session = self.session()?;
        let entry_id = session
            .session_manager
            .append_custom_entry(custom_type, data)?;
        if let Some(entry) = session.session_manager.get_entry(&entry_id) {
            session.emit(AgentSessionEvent::EntryAppended { entry });
        }
        Ok(())
    }

    fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        self.session()?.set_session_name(name);
        Ok(())
    }

    fn get_session_name(&self) -> anyhow::Result<Option<String>> {
        Ok(self.session()?.session_manager.get_session_name())
    }

    fn set_label(&self, entry_id: &str, label: Option<&str>) -> anyhow::Result<()> {
        self.session()?
            .session_manager
            .append_label_change(entry_id, label)?;
        Ok(())
    }

    fn get_active_tools(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.session()?.get_active_tool_names())
    }

    fn get_all_tools(&self) -> anyhow::Result<Vec<ToolInfo>> {
        Ok(self.session()?.get_all_tools())
    }

    fn get_settings(&self) -> anyhow::Result<Settings> {
        Ok(self.session()?.settings_manager.get_settings())
    }

    fn set_active_tools(&self, tool_names: Vec<String>) -> anyhow::Result<()> {
        self.session()?.set_active_tools_by_name(&tool_names);
        Ok(())
    }

    fn refresh_tools(&self) -> anyhow::Result<()> {
        self.session()?.refresh_tool_registry(None, false);
        Ok(())
    }

    fn get_commands(&self) -> anyhow::Result<Vec<SlashCommandInfo>> {
        Ok(self.session()?.get_commands())
    }

    async fn set_model(&self, model: Model) -> anyhow::Result<bool> {
        let session = self.session()?;
        if !session.model_runtime.has_configured_auth(&model.provider) {
            return Ok(false);
        }
        session.set_model(model, None).await?;
        Ok(true)
    }

    fn get_thinking_level(&self) -> anyhow::Result<ThinkingLevel> {
        Ok(self.session()?.thinking_level())
    }

    fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        self.session()?.set_thinking_level(level, None);
        Ok(())
    }
}

/// `ctx.*` actions backed by the session (`bindCore()` context actions).
struct SessionContextActions {
    session: Weak<AgentSession>,
}

#[async_trait::async_trait]
impl ExtensionContextActions for SessionContextActions {
    fn get_model(&self) -> Option<Model> {
        self.session.upgrade()?.model()
    }

    fn get_scoped_models(&self) -> Vec<ScopedModel> {
        self.session
            .upgrade()
            .map(|s| {
                s.scoped_models()
                    .into_iter()
                    .map(|m| ScopedModel {
                        model: m.model,
                        thinking_level: m.thinking_level,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_idle(&self) -> bool {
        self.session.upgrade().is_none_or(|s| s.is_idle())
    }

    fn is_project_trusted(&self) -> bool {
        self.session
            .upgrade()
            .is_some_and(|s| s.settings_manager.is_project_trusted())
    }

    fn get_signal(&self) -> Option<CancellationToken> {
        self.session.upgrade()?.agent.signal()
    }

    fn abort(&self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let handler = session.state.lock().extension_abort_handler.clone();
        if let Some(handler) = handler {
            handler();
            return;
        }
        spawn_detached(async move {
            let _ = session.abort().await;
        });
    }

    fn has_pending_messages(&self) -> bool {
        self.session
            .upgrade()
            .is_some_and(|s| s.pending_message_count() > 0)
    }

    fn shutdown(&self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let handler = session.state.lock().extension_shutdown_handler.clone();
        if let Some(handler) = handler {
            handler();
        }
    }

    fn get_context_usage(&self) -> Option<ContextUsage> {
        self.session.upgrade()?.get_context_usage()
    }

    fn compact(&self, options: CompactOptions) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        spawn_detached(async move {
            match session.compact(options.custom_instructions.as_deref()).await {
                Ok(result) => {
                    if let Some(on_complete) = &options.on_complete {
                        on_complete(result);
                    }
                }
                Err(error) => {
                    if let Some(on_error) = &options.on_error {
                        on_error(Arc::new(error));
                    }
                }
            }
        });
    }

    fn get_system_prompt(&self) -> String {
        self.session
            .upgrade()
            .map(|s| s.system_prompt())
            .unwrap_or_default()
    }

    fn get_system_prompt_options(&self) -> Option<BuildSystemPromptOptions> {
        let session = self.session.upgrade()?;
        let options = session.state.lock().base_system_prompt_options.clone();
        Some(options.into())
    }

    fn execute_tool_fn(&self) -> Option<ExecuteToolFn> {
        let weak = self.session.clone();
        Some(Arc::new(move |caller_id, name, args, options| {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(session) => {
                        session
                            .execute_nested_tool_call(caller_id, name, args, options)
                            .await
                    }
                    None => AgentToolCallOutcome {
                        tool_call: AgentToolCall {
                            id: format!("{caller_id}/0"),
                            name,
                            ..Default::default()
                        },
                        result: AgentToolResult::error("Session is disposed"),
                        is_error: true,
                        duration_ms: None,
                    },
                }
            })
        }))
    }

    fn get_callable_tools(&self) -> Option<Vec<AgentToolRef>> {
        Some(self.session.upgrade()?.get_callable_tools(None))
    }
}

/// Messaging of a [`ReplacedSessionContext`] bound to this session.
struct SessionReplacedActions {
    session: Weak<AgentSession>,
}

#[async_trait::async_trait]
impl ReplacedSessionActions for SessionReplacedActions {
    async fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| err("The session for this context was disposed"))?;
        session
            .send_custom_message(
                &AgentSessionSendCustomMessageMessage {
                    custom_type: message.custom_type,
                    content: message.content,
                    display: message.display,
                    details: message.details,
                },
                Some(&AgentSessionSendCustomMessageOptions {
                    trigger_turn: options.trigger_turn,
                    deliver_as: options.deliver_as.map(|d| match d {
                        DeliverAs::Steer => AgentSessionSendCustomMessageOptionsDeliverAs::Steer,
                        DeliverAs::FollowUp => AgentSessionSendCustomMessageOptionsDeliverAs::FollowUp,
                        DeliverAs::NextTurn => AgentSessionSendCustomMessageOptionsDeliverAs::NextTurn,
                    }),
                }),
            )
            .await
    }

    async fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| err("The session for this context was disposed"))?;
        session
            .send_user_message(
                content,
                Some(&AgentSessionSendUserMessageOptions {
                    deliver_as: options.deliver_as.map(|d| match d {
                        StreamingBehavior::Steer => PromptOptionsStreamingBehavior::Steer,
                        StreamingBehavior::FollowUp => PromptOptionsStreamingBehavior::FollowUp,
                    }),
                    expand_prompt_templates: options.expand_prompt_templates,
                }),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_blocks() {
        let parsed = parse_skill_block(
            "<skill name=\"demo\" location=\"/skills/demo/SKILL.md\">\nbody line\nmore\n</skill>\n\n  user text  ",
        )
        .unwrap();
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.location, "/skills/demo/SKILL.md");
        assert_eq!(parsed.content, "body line\nmore");
        assert_eq!(parsed.user_message.as_deref(), Some("user text"));

        let no_message =
            parse_skill_block("<skill name=\"a\" location=\"b\">\nc\n</skill>").unwrap();
        assert_eq!(no_message.user_message, None);
        assert!(parse_skill_block("plain text").is_none());
        assert!(parse_skill_block("<skill name=\"a\" location=\"b\">\nc\n</skill>trailing").is_none());
    }

    #[test]
    fn extension_source_labels() {
        assert_eq!(
            AgentSession::get_extension_source_label("/x/my-ext.ts"),
            "extension:my-ext"
        );
        assert_eq!(
            AgentSession::get_extension_source_label("<inline:foo>"),
            "extension:inline:foo"
        );
    }

    #[test]
    fn prompt_snippets_and_guidelines_normalize() {
        assert_eq!(
            AgentSession::normalize_prompt_snippet(Some("  a\n\nb   c ")),
            Some("a b c".into())
        );
        assert_eq!(AgentSession::normalize_prompt_snippet(Some(" \n ")), None);
        assert_eq!(
            AgentSession::normalize_prompt_guidelines(Some(&[
                " x ".into(),
                "x".into(),
                " ".into(),
                "y".into()
            ])),
            vec!["x".to_string(), "y".to_string()]
        );
    }
}
