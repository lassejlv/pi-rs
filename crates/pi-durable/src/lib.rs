//! Native port of pi-durable 1.4.0. Storage is independent of coding-agent sessions.
//!
//! On wasm32 tools, images and the native environments (process, filesystem,
//! watch) are compiled out because they need OS access; the harness, scheduler,
//! storage (including IndexedDB) and Worker hosting build there.

pub mod codemode;
pub mod documents;
pub mod entries;
pub mod env;
pub mod errors;
pub mod harness;
#[cfg(not(target_arch = "wasm32"))]
pub mod images;
pub mod search;
pub mod session;
pub mod storage;
pub mod tasks;
pub mod testing;
pub mod timers;
pub mod tool_search;
pub use harness::settled::SettledTask;
#[cfg(not(target_arch = "wasm32"))]
pub mod tools;
pub mod truncate;
pub mod worker;
pub mod types;
mod wire;
pub use documents::*;
pub use entries::*;
pub use errors::{Error, ErrorKind, Result};
pub use harness::agent::{AGENT_DOC, Agent, AgentState, AgentView, configure};
pub use harness::compaction::{
    COMPACTION_TASK, CompactionCheckpoint, CompactionHooks, CompactionInput, CompactionPolicy,
    CompactionResult, SummaryRequest, compaction_task,
};
pub use harness::context::ContextView;
pub use harness::conversation::{ConversationHandle, InputSubmissionDraft};
pub use harness::declaration::{DeclaredTool as ToolDeclarationProvider, ToolDeclaration};
pub use harness::define::{
    SectionOptions, define_extension, define_tool, hook, section, wrap_section, wrap_tool,
};
pub use harness::events::{
    AgentEvent, AgentEventStream, MessageChange, SnapshotEvent, ToolEventCall, watch_events,
};
pub use harness::generation::{
    GenerationCheckpoint, GenerationHooks, GenerationInput, GenerationResult, GenerationTask,
    generation_task,
};
pub use harness::hooks::{HookKey, HookRunner};
pub use harness::hook_api::{DocumentReader, HookApi};
pub use harness::inbox::{INBOX_DOC, InboxItem, InboxState, inbox_doc};
pub use harness::inspect::{HarnessInspection, Scheduling, TaskInspection, TaskInspectionState};
pub use harness::live::{
    CompactionFailure, CompactionReason, CompactionStatus, LIVE_DOC, LiveState, NestedToolSlot,
    NestedToolSummary, ToolDiagnostic, ToolSlot, live_doc,
};
pub use harness::prompt::{
    NativePromptInput, NativeSection, NativeSectionRender, PromptInput, PromptSection,
};
pub use harness::provider::{PROVIDER_DOC, ProviderState, provider_doc};
pub use harness::reader::{RegistryReader, RegistryView};
pub use harness::registry::{
    Extension, HookRegistration, RegisteredSection, RegisteredTool, Registry, RegistrySnapshot,
    Wrap,
};
pub use harness::scheduler::{BlockedReason, ReportTask, TaskReport as ReportInfo, Wake};
pub use harness::settings::{
    DEFAULT_COMPACTION_POLICY, DEFAULT_PROGRESS_POLICY, DEFAULT_RESTART_LIMIT,
    DEFAULT_RETRY_POLICY, HarnessSettings, ProgressPolicy, Settings,
};
pub use harness::submissions::{Submission, SubmissionAbort, Submissions, SubmissionsOptions};
pub use harness::task_graph::{
    TaskGraph, TaskGraphNode, TaskGraphOutcome, TaskGraphState, TaskGraphWatch,
};
pub use harness::telemetry::DURABLE_TELEMETRY_SCHEMA;
pub use harness::tool::{
    NESTED_CALL_DOC, NativeTool, NestedCallState, NestedToolExecutionResult, TOOL_TASK,
    ToolControl, ToolExecutionApi, ToolExecutionMode, ToolExecutionResult, ToolHookCall, ToolHooks,
    ToolTask, ToolTaskCheckpoint, ToolTaskInput, ToolTaskResult, tool_task,
};
pub use harness::typed::{
    AfterFailedRun, AgentChange, CompactionPolicyPatch, ModelChange, CompactionSettings, ConversationRetryPolicy,
    ConversationRetryPolicyPatch, ConversationStreamOptions, NameSelection, ToolSelection,
};
pub use harness::types::{
    InputEntry, ModelRef, QueueMode, QueueModes, SettledSubmissionRecord, SubmissionDraft,
    UserInput, WhenBusy,
};
pub use harness::usage::{USAGE_DOC, UsageState, usage_doc};
pub use harness::view::{ConversationView, ConversationViewWatch};
pub use harness::{
    Conversation, ConversationCreateOptions, ConversationCreated, ConversationEntryQuery,
    ConversationInit, ConversationOptions, EnvTarget, EnvironmentFactory, Harness, HarnessOptions,
    create_registry, default_harness_identity,
};
pub use harness::{runtime::TaskRuntime, state::NextTaskState};
pub use pi_chord::{Context, JsonString, JsonValue, Watch as WatchHandle, WatchEnd};
pub use session::*;
pub use storage::{
    JsonlStorage, JsonlStorageOptions, MemoryStorage, SqliteStorage, SqliteStorageOptions, Storage,
};
pub use timers::SetTimer;
pub use tasks::{
    AnyTask, PhaseHandler, RunningTask, Task, TaskDefinition, TaskValue, TypedTaskId, define_task,
};
pub use types::*;
