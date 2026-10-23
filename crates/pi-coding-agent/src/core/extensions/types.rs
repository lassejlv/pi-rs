//! Port of `packages/coding-agent/src/core/extensions/types.ts`: the extension API contract.
//!
//! Built-in Rust extensions (`crate::extensions::*`) and JavaScript extensions (run by `pi-jsrt`
//! through a bridge) both target the types in this module.
//!
//! # Design
//!
//! **Events.** Every event an extension can observe is a struct (`SessionStartEvent`,
//! `ToolCallEvent`, ...) and a variant of [`ExtensionEvent`], which is serde-tagged by `type`
//! exactly like the TypeScript union, so a JS bridge can marshal events with `serde_json`.
//! Member structs do not contain the `type` field: the enum supplies it. Fields that are not
//! JSON (`signal`) are `#[serde(skip)]`; the bridge passes them separately.
//!
//! **Handlers.** A handler is an [`ExtensionHandler`]:
//! `Arc<dyn Fn(ExtensionEvent, ExtensionContext) -> BoxFuture<'static, anyhow::Result<Option<ExtensionEventResult>>>>`.
//! One closure type serves Rust and JS: Rust handlers return a typed [`ExtensionEventResult`]
//! variant, the JS bridge returns [`ExtensionEventResult::Json`] with whatever the JS handler
//! returned, and the runner converts with [`EventResultType::from_result`]. For typed Rust
//! registration use [`on_event`] (or [`ExtensionEventPayload`] directly):
//!
//! ```ignore
//! on_event::<SessionStartEvent, _>(&*pi, |event, ctx| async move {
//!     ctx.ui()?.notify(&format!("started: {:?}", event.reason), NotifyType::Info);
//!     Ok(None)
//! })?;
//! ```
//!
//! **In-place mutation.** TypeScript handlers mutate some events in place. In Rust the event is
//! passed by value, and mutations are returned instead:
//! - `tool_call`: [`ToolCallEventResult::input`] replaces the arguments (later handlers see it).
//! - `before_provider_headers`: [`BeforeProviderHeadersEventResult::headers`] replaces the headers.
//! - `before_agent_start`: [`BeforeAgentStartEventResult::system_prompt_options`] replaces the
//!   prompt sections.
//!
//! The JS bridge reads the mutated JS object back after the handler returns and fills these fields.
//!
//! **Contexts.** [`ExtensionContext`] is a cheap, cloneable handle over an
//! [`ExtensionContextHost`] (implemented by the `ExtensionRunner`). Every accessor first checks
//! that the runtime is still active and returns an error for stale contexts, matching the
//! throwing getters in `runner.ts`. [`ExtensionToolContext`], [`ExtensionCommandContext`] and
//! [`ReplacedSessionContext`] wrap it and `Deref` to it.
//!
//! **The `pi` object.** [`ExtensionAPI`] is a trait (implemented in `loader.rs`); extension
//! factories receive an [`ExtensionAPIRef`]. All methods return `anyhow::Result` because every
//! TypeScript method throws when the extension instance is stale.
//!
//! **UI.** [`ExtensionUIContext`] is a trait implemented per mode (interactive, RPC, print/no-op).
//! Custom components are `pi_tui::tui::ComponentRef`; TS `dispose()` maps to `Drop`.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use pi_agent::types::{
    AgentMessage, AgentToolCallOutcome, AgentToolRef, AgentToolResult, AgentToolUpdateCallback,
    ThinkingLevel, ToolExecutionMode,
};
use pi_ai::models::{Provider, ProviderClassifier, ProviderImages, RefreshModelsContext};
use pi_ai::types::{
    Api, AssistantMessageEvent, ConstrainedSamplingConfig, ImageContent, JsonObject, Message,
    Model, ModelCompat, ModelCost, ModelInputLimits, ModelInputModality, ModelPromptCache,
    ProviderHeaders, ProviderId, ToolResultMessage, TranscriptContext, Usage, UserContent,
    UserContentBlock,
};
use pi_ai::utils::event_stream::AssistantMessageEventStream;
use pi_tui::tui::{ComponentRef, OverlayHandle, OverlayOptions, Tui};

use crate::compat::{
    AutocompleteItem, AutocompleteProvider, EditorComponent, EditorTheme, OAuthLoginCallbacks,
};
use crate::core::bash_executor::BashResult;
use crate::core::cache_warmer::{CacheWarmingDecisionEvent, CacheWarmingDecisionEventResult};
use crate::core::compaction::compaction::{CompactionPreparation, CompactionResult};
use crate::core::event_bus::EventBus;
use crate::core::footer_data_provider::ReadonlyFooterDataProvider;
use crate::core::mcp_servers::{McpServerConfig, McpServerRegistry, RegisteredMcpServer};
use crate::core::messages::CustomMessage;
use crate::core::model_registry::ModelRegistry;
use crate::core::model_resolver::ScopedModel;
use crate::core::session_manager::{
    BranchSummaryEntry, CompactionEntry, ContextEditEntryReplacement, CustomEntry,
    ProjectedSessionEntry, ReadonlySessionManager, SessionEntry, SessionManager,
};
use crate::core::settings_manager::Settings;
use crate::core::slash_commands::SlashCommandInfo;
use crate::core::source_info::SourceInfo;
use crate::core::tools::bash::BashOperations;
use crate::core::tools::edit::EditToolDetails;
use crate::core::tools::{
    BashToolDetails, BashToolInput, EditToolInput, FindToolDetails, FindToolInput, GrepToolDetails,
    GrepToolInput, LsToolDetails, LsToolInput, PowerShellToolDetails, PowerShellToolInput,
    ReadToolDetails, ReadToolInput, WriteToolInput,
};
use crate::core::virtual_models::{ModelRoute, ModelRouteRequest, VirtualModelDefinition};
use crate::modes::interactive::theme::theme::Theme;

pub use crate::core::exec::{ExecOptions, ExecResult};
pub use crate::core::keybindings::{AppKeybinding, KeybindingsManager};
pub use crate::core::system_prompt::{
    BuildSystemPromptOptions, NormalizedBuildSystemPromptOptions,
};

/// Removes a registration (TS: the `() => void` returned by `on()`, `onTerminalInput()`, ...).
pub type Unsubscribe = Arc<dyn Fn() + Send + Sync>;

/// A `KeyId` from pi-tui, e.g. `"ctrl+shift+p"`.
pub type KeyId = String;

/// Arbitrary result passed to `done()` of a custom UI component (TS generic `T`).
/// Rust callers downcast it; the JS bridge stores a handle to the JS value.
pub type CustomUiResult = Box<dyn Any + Send>;

// ============================================================================
// UI Context
// ============================================================================

/// Options for extension UI dialogs.
#[derive(Debug, Clone, Default)]
pub struct ExtensionUIDialogOptions {
    /// Signal to programmatically dismiss the dialog.
    pub signal: Option<CancellationToken>,
    /// Timeout in milliseconds. Dialog auto-dismisses with live countdown display.
    pub timeout: Option<u64>,
}

/// Placement for extension widgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WidgetPlacement {
    #[default]
    #[serde(rename = "aboveEditor")]
    AboveEditor,
    #[serde(rename = "belowEditor")]
    BelowEditor,
}

/// Options for extension widgets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionWidgetOptions {
    /// Where the widget is rendered. Defaults to "aboveEditor".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<WidgetPlacement>,
}

/// Result of a [`TerminalInputHandler`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputHandlerResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consume: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Raw terminal input listener for extensions.
pub type TerminalInputHandler =
    Arc<dyn Fn(&str) -> Option<TerminalInputHandlerResult> + Send + Sync>;

/// Working indicator configuration for the interactive streaming loader.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingIndicatorOptions {
    /// Animation frames. Use an empty array to hide the indicator entirely. Custom frames are rendered verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds for animated indicators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

/// Wrap the current autocomplete provider with additional behavior.
pub type AutocompleteProviderFactory =
    Arc<dyn Fn(Arc<dyn AutocompleteProvider>) -> Arc<dyn AutocompleteProvider> + Send + Sync>;

/// Creates a custom editor component (`setEditorComponent`).
pub type EditorFactory = Arc<
    dyn Fn(Arc<Tui>, EditorTheme, Arc<KeybindingsManager>) -> Arc<dyn EditorComponent>
        + Send
        + Sync,
>;

/// Notification severity for `ui.notify()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyType {
    #[default]
    Info,
    Warning,
    Error,
}

/// Creates a widget, header or other themed component.
pub type ComponentFactory = Arc<dyn Fn(Arc<Tui>, Arc<Theme>) -> ComponentRef + Send + Sync>;

/// Creates a custom footer component.
pub type FooterFactory =
    Arc<dyn Fn(Arc<Tui>, Arc<Theme>, ReadonlyFooterDataProvider) -> ComponentRef + Send + Sync>;

/// Content of `setWidget()`: lines of text or a component factory.
#[derive(Clone)]
pub enum WidgetContent {
    Lines(Vec<String>),
    Component(ComponentFactory),
}

/// The `done` callback handed to a custom component factory.
pub type CustomUiDone = Arc<dyn Fn(CustomUiResult) + Send + Sync>;

/// Factory of `ui.custom()`. The TS factory may return a component or a promise of one.
pub type CustomComponentFactory = Arc<
    dyn Fn(
            Arc<Tui>,
            Arc<Theme>,
            Arc<KeybindingsManager>,
            CustomUiDone,
        ) -> BoxFuture<'static, anyhow::Result<ComponentRef>>
        + Send
        + Sync,
>;

/// Overlay options of `ui.custom()`: static or computed on every layout.
#[derive(Clone)]
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub enum OverlayOptionsSource {
    Static(OverlayOptions),
    Dynamic(Arc<dyn Fn() -> OverlayOptions + Send + Sync>),
}

/// Options of `ui.custom()`.
#[derive(Clone, Default)]
pub struct CustomUiOptions {
    pub overlay: Option<bool>,
    /// Overlay positioning/sizing options. Can be static or a function for dynamic updates.
    pub overlay_options: Option<OverlayOptionsSource>,
    /// Called with the overlay handle after the overlay is shown. Use to control visibility.
    pub on_handle: Option<Arc<dyn Fn(OverlayHandle) + Send + Sync>>,
}

/// Entry of `ui.getAllThemes()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<String>,
}

/// Argument of `ui.setTheme()`: a theme name or a theme object.
#[derive(Clone)]
pub enum ThemeSelection {
    Name(String),
    Theme(Arc<Theme>),
}

/// Result of `ui.setTheme()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetThemeResult {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// UI context for extensions to request interactive UI.
/// Each mode (interactive, RPC, print) provides its own implementation.
#[async_trait::async_trait]
pub trait ExtensionUIContext: Send + Sync {
    /// Show a selector and return the user's choice.
    async fn select(
        &self,
        title: &str,
        options: Vec<String>,
        opts: ExtensionUIDialogOptions,
    ) -> Option<String>;

    /// Show a confirmation dialog.
    async fn confirm(&self, title: &str, message: &str, opts: ExtensionUIDialogOptions) -> bool;

    /// Show a text input dialog.
    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: ExtensionUIDialogOptions,
    ) -> Option<String>;

    /// Show a notification to the user.
    fn notify(&self, message: &str, kind: NotifyType);

    /// Listen to raw terminal input (interactive mode only). Returns an unsubscribe function.
    fn on_terminal_input(&self, handler: TerminalInputHandler) -> Unsubscribe;

    /// Set status text in the footer/status bar. Pass `None` to clear.
    fn set_status(&self, key: &str, text: Option<&str>);

    /// Set the working/loading message shown during streaming. `None` restores the default.
    fn set_working_message(&self, message: Option<&str>);

    /// Show or hide the built-in interactive working loader row during streaming.
    fn set_working_visible(&self, visible: bool);

    /// Configure the interactive working indicator shown during streaming.
    ///
    /// - `None` restores the default animated spinner.
    /// - `frames: ["●"]` gives a static indicator.
    /// - `frames: []` hides the indicator entirely.
    /// - Custom frames are rendered as provided, so extensions must add their own colors.
    fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>);

    /// Set the label shown for hidden thinking blocks. `None` restores the default.
    fn set_hidden_thinking_label(&self, label: Option<&str>);

    /// Set a widget to display above or below the editor. `None` removes it.
    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: ExtensionWidgetOptions,
    );

    /// Set a custom footer component, or `None` to restore the built-in footer.
    ///
    /// The factory receives a footer data provider for data not otherwise accessible:
    /// git branch and extension statuses from `set_status()`.
    fn set_footer(&self, factory: Option<FooterFactory>);

    /// Set a custom header component (shown at startup, above chat), or `None` to restore the built-in header.
    fn set_header(&self, factory: Option<ComponentFactory>);

    /// Set the terminal window/tab title.
    fn set_title(&self, title: &str);

    /// Show a custom component with keyboard focus. Resolves with the value passed to `done`.
    async fn custom(
        &self,
        factory: CustomComponentFactory,
        options: CustomUiOptions,
    ) -> anyhow::Result<CustomUiResult>;

    /// Paste text into the editor, triggering paste handling (collapse for large content).
    fn paste_to_editor(&self, text: &str);

    /// Set the text in the core input editor.
    fn set_editor_text(&self, text: &str);

    /// Get the current text from the core input editor.
    fn get_editor_text(&self) -> String;

    /// Show a multi-line editor for text editing.
    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String>;

    /// Stack additional autocomplete behavior on top of the built-in provider.
    fn add_autocomplete_provider(&self, factory: AutocompleteProviderFactory);

    /// Set a custom editor component via factory function. `None` restores the default editor.
    fn set_editor_component(&self, factory: Option<EditorFactory>);

    /// Get the currently configured custom editor factory, or `None` when using the default editor.
    fn get_editor_component(&self) -> Option<EditorFactory>;

    /// The current theme for styling (TS: `readonly theme`).
    fn theme(&self) -> Arc<Theme>;

    /// Get all available themes with their names and file paths.
    fn get_all_themes(&self) -> Vec<ThemeInfo>;

    /// Load a theme by name without switching to it. Returns `None` if not found.
    fn get_theme(&self, name: &str) -> Option<Arc<Theme>>;

    /// Set the current theme by name or theme object.
    fn set_theme(&self, theme: ThemeSelection) -> SetThemeResult;

    /// Get current tool output expansion state.
    fn get_tools_expanded(&self) -> bool;

    /// Set tool output expansion state.
    fn set_tools_expanded(&self, expanded: bool);
}

/// `ui.custom()` with a typed result: downcasts the value passed to `done`.
pub async fn custom_typed<T: Send + 'static>(
    ui: &dyn ExtensionUIContext,
    factory: CustomComponentFactory,
    options: CustomUiOptions,
) -> anyhow::Result<T> {
    let value = ui.custom(factory, options).await?;
    value.downcast::<T>().map(|b| *b).map_err(|_| {
        anyhow::anyhow!("custom UI component finished with a value of an unexpected type")
    })
}

// ============================================================================
// Extension Context
// ============================================================================

/// Context usage of the active model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Estimated context tokens, or `None` (JSON `null`) if unknown (e.g. right after compaction,
    /// before the next LLM response).
    pub tokens: Option<u64>,
    pub context_window: u64,
    /// Context usage as percentage of context window, or `None` if tokens is unknown.
    #[serde(with = "pi_ai::serde_util::opt_js_f64")]
    pub percent: Option<f64>,
}

/// Options of `ctx.compact()`.
#[derive(Clone, Default)]
pub struct CompactOptions {
    pub custom_instructions: Option<String>,
    pub on_complete: Option<Arc<dyn Fn(CompactionResult) + Send + Sync>>,
    pub on_error: Option<Arc<dyn Fn(Arc<anyhow::Error>) + Send + Sync>>,
}

/// Current run mode. Use [`ExtensionMode::Tui`] to guard terminal-only UI such as custom components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Rpc => "rpc",
            Self::Json => "json",
            Self::Print => "print",
        }
    }
}

/// Backend of [`ExtensionContext`]: implemented by the `ExtensionRunner` (`runner.rs`).
/// Accessors are evaluated lazily on every call, like the getters of `runner.createContext()`.
pub trait ExtensionContextHost: Send + Sync {
    /// Errors when the extension runtime was invalidated (reload, session replacement).
    fn assert_active(&self) -> anyhow::Result<()>;
    fn ui(&self) -> Arc<dyn ExtensionUIContext>;
    fn mode(&self) -> ExtensionMode;
    fn has_ui(&self) -> bool;
    fn cwd(&self) -> String;
    /// `None` where the context has no session (e.g. `project_trust` handlers).
    fn session_manager(&self) -> Option<ReadonlySessionManager>;
    /// `None` where the context has no model registry (e.g. `project_trust` handlers).
    fn model_registry(&self) -> Option<Arc<ModelRegistry>>;
    /// `runtime.getThinkingLevel()`; `None` before the session runtime binds.
    fn thinking_level(&self) -> Option<ThinkingLevel>;
    /// Session-provided actions (`bindCore()`).
    fn actions(&self) -> Arc<dyn ExtensionContextActions>;
}

/// Context passed to extension event handlers (TS `ExtensionContext`).
///
/// Cheap to clone. Every accessor fails once the extension runtime is stale.
#[derive(Clone)]
pub struct ExtensionContext {
    host: Arc<dyn ExtensionContextHost>,
}

impl fmt::Debug for ExtensionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionContext").finish_non_exhaustive()
    }
}

impl ExtensionContext {
    pub fn new(host: Arc<dyn ExtensionContextHost>) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &Arc<dyn ExtensionContextHost> {
        &self.host
    }

    /// UI methods for user interaction.
    pub fn ui(&self) -> anyhow::Result<Arc<dyn ExtensionUIContext>> {
        self.host.assert_active()?;
        Ok(self.host.ui())
    }

    /// Current run mode.
    pub fn mode(&self) -> anyhow::Result<ExtensionMode> {
        self.host.assert_active()?;
        Ok(self.host.mode())
    }

    /// Whether dialog-capable UI is available (true in TUI and RPC modes).
    pub fn has_ui(&self) -> anyhow::Result<bool> {
        self.host.assert_active()?;
        Ok(self.host.has_ui())
    }

    /// Current working directory.
    pub fn cwd(&self) -> anyhow::Result<String> {
        self.host.assert_active()?;
        Ok(self.host.cwd())
    }

    /// Session manager (read-only).
    pub fn session_manager(&self) -> anyhow::Result<ReadonlySessionManager> {
        self.host.assert_active()?;
        self.host
            .session_manager()
            .ok_or_else(|| anyhow::anyhow!("sessionManager is not available in this context"))
    }

    /// Model registry for API key resolution.
    pub fn model_registry(&self) -> anyhow::Result<Arc<ModelRegistry>> {
        self.host.assert_active()?;
        self.host
            .model_registry()
            .ok_or_else(|| anyhow::anyhow!("modelRegistry is not available in this context"))
    }

    /// Current model (may be `None`).
    pub fn model(&self) -> anyhow::Result<Option<Model>> {
        self.host.assert_active()?;
        Ok(self.host.actions().get_model())
    }

    /// Models scoped to this session (from `--models` / `enabledModels`). Empty when no scoping is configured.
    pub fn scoped_models(&self) -> anyhow::Result<Vec<ScopedModel>> {
        self.host.assert_active()?;
        Ok(self.host.actions().get_scoped_models())
    }

    /// Current thinking level, when provided by the session runtime.
    pub fn thinking_level(&self) -> anyhow::Result<Option<ThinkingLevel>> {
        self.host.assert_active()?;
        Ok(self.host.thinking_level())
    }

    /// Whether the agent is idle (not streaming).
    pub fn is_idle(&self) -> anyhow::Result<bool> {
        self.host.assert_active()?;
        Ok(self.host.actions().is_idle())
    }

    /// Whether project-local trust is active for this context.
    pub fn is_project_trusted(&self) -> anyhow::Result<bool> {
        self.host.assert_active()?;
        Ok(self.host.actions().is_project_trusted())
    }

    /// The current abort signal, or `None` when the agent is not streaming.
    pub fn signal(&self) -> anyhow::Result<Option<CancellationToken>> {
        self.host.assert_active()?;
        Ok(self.host.actions().get_signal())
    }

    /// Abort the current agent operation.
    pub fn abort(&self) -> anyhow::Result<()> {
        self.host.assert_active()?;
        self.host.actions().abort();
        Ok(())
    }

    /// Whether there are queued messages waiting.
    pub fn has_pending_messages(&self) -> anyhow::Result<bool> {
        self.host.assert_active()?;
        Ok(self.host.actions().has_pending_messages())
    }

    /// Gracefully shutdown pi and exit. Available in all contexts.
    pub fn shutdown(&self) -> anyhow::Result<()> {
        self.host.assert_active()?;
        self.host.actions().shutdown();
        Ok(())
    }

    /// Get current context usage for the active model.
    pub fn get_context_usage(&self) -> anyhow::Result<Option<ContextUsage>> {
        self.host.assert_active()?;
        Ok(self.host.actions().get_context_usage())
    }

    /// Trigger compaction without awaiting completion.
    pub fn compact(&self, options: CompactOptions) -> anyhow::Result<()> {
        self.host.assert_active()?;
        self.host.actions().compact(options);
        Ok(())
    }

    /// Get the current effective system prompt.
    pub fn get_system_prompt(&self) -> anyhow::Result<String> {
        self.host.assert_active()?;
        Ok(self.host.actions().get_system_prompt())
    }
}

/// Options for [`ExtensionToolContext::execute_tool`].
#[derive(Clone, Default)]
pub struct ExecuteToolOptions {
    /// Defaults to the calling tool's signal.
    pub signal: Option<CancellationToken>,
    /// Receives partial results of the nested tool, in addition to `tool_execution_update` events.
    pub on_update: Option<AgentToolUpdateCallback>,
}

/// Context passed to tool `execute()` in a session: the extension context plus `execute_tool()`
/// for running other tools through the same validation, hooks, and permission checks as
/// model-issued calls. Created by `runner.createToolContext(toolCallId, signal)`.
#[derive(Clone)]
pub struct ExtensionToolContext {
    pub ctx: ExtensionContext,
    /// Id of the calling tool call; nested calls get `<tool_call_id>/<n>`.
    pub tool_call_id: String,
    /// Default signal of nested calls.
    pub signal: Option<CancellationToken>,
}

impl Deref for ExtensionToolContext {
    type Target = ExtensionContext;
    fn deref(&self) -> &ExtensionContext {
        &self.ctx
    }
}

impl ExtensionToolContext {
    /// Tools [`Self::execute_tool`] can call.
    pub fn tools(&self) -> anyhow::Result<Vec<AgentToolRef>> {
        self.ctx.host.assert_active()?;
        Ok(self
            .ctx
            .host
            .actions()
            .get_callable_tools()
            .unwrap_or_default())
    }

    /// Run another tool. The call gets the id `<calling id>/<n>`, and the `tool_call`,
    /// `tool_result`, and `tool_execution_*` events carry `parentToolCallId`.
    ///
    /// Never fails for tool failures: unknown tools, validation errors, blocked calls, and thrown
    /// errors come back with `is_error: true`. Fails only for stale contexts.
    pub async fn execute_tool(
        &self,
        name: &str,
        args: Value,
        options: ExecuteToolOptions,
    ) -> anyhow::Result<AgentToolCallOutcome> {
        self.ctx.host.assert_active()?;
        let Some(execute) = self.ctx.host.actions().execute_tool_fn() else {
            let details = serde_json::Map::new();
            return Ok(AgentToolCallOutcome {
                tool_call: pi_ai::types::ToolCall {
                    id: format!("{}/0", self.tool_call_id),
                    name: name.to_string(),
                    ..Default::default()
                },
                result: AgentToolResult {
                    content: vec![UserContentBlock::text(
                        "Nested tool calls are not available in this context",
                    )],
                    details: Some(Value::Object(details)),
                    ..Default::default()
                },
                is_error: true,
                duration_ms: None,
            });
        };
        let options = ExecuteToolOptions {
            signal: options.signal.or_else(|| self.signal.clone()),
            on_update: options.on_update,
        };
        Ok(execute(self.tool_call_id.clone(), name.to_string(), args, options).await)
    }
}

/// Options of `ctx.newSession()`.
#[derive(Clone, Default)]
pub struct NewSessionOptions {
    pub parent_session: Option<String>,
    pub setup: Option<
        Arc<dyn Fn(Arc<SessionManager>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>,
    >,
    pub with_session: Option<WithSessionFn>,
}

/// Callback receiving a context bound to the replacement session.
pub type WithSessionFn =
    Arc<dyn Fn(ReplacedSessionContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Where `fork()` branches relative to the entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForkPosition {
    #[default]
    Before,
    At,
}

/// Options of `ctx.fork()`.
#[derive(Clone, Default)]
pub struct ForkOptions {
    pub position: Option<ForkPosition>,
    pub with_session: Option<WithSessionFn>,
}

/// Options of `ctx.navigateTree()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTreeOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Options of `ctx.switchSession()`.
#[derive(Clone, Default)]
pub struct SwitchSessionOptions {
    pub with_session: Option<WithSessionFn>,
}

/// `{ cancelled: boolean }` returned by session-replacing commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReplaceOutcome {
    pub cancelled: bool,
}

/// Extended context for command handlers.
/// Includes session control methods only safe in user-initiated commands.
#[derive(Clone)]
pub struct ExtensionCommandContext {
    pub ctx: ExtensionContext,
    pub command_actions: Arc<dyn ExtensionCommandContextActions>,
}

impl Deref for ExtensionCommandContext {
    type Target = ExtensionContext;
    fn deref(&self) -> &ExtensionContext {
        &self.ctx
    }
}

impl ExtensionCommandContext {
    /// Get the current base system-prompt construction options.
    pub fn get_system_prompt_options(&self) -> anyhow::Result<BuildSystemPromptOptions> {
        self.ctx.host.assert_active()?;
        self.ctx
            .host
            .actions()
            .get_system_prompt_options()
            .ok_or_else(|| {
                anyhow::anyhow!("getSystemPromptOptions is not available in this context")
            })
    }

    /// Wait for the agent to finish streaming.
    pub async fn wait_for_idle(&self) -> anyhow::Result<()> {
        self.ctx.host.assert_active()?;
        self.command_actions.wait_for_idle().await
    }

    /// Start a new session, optionally with initialization.
    pub async fn new_session(
        &self,
        options: NewSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        self.ctx.host.assert_active()?;
        self.command_actions.new_session(options).await
    }

    /// Fork from a specific entry, creating a new session file.
    pub async fn fork(
        &self,
        entry_id: &str,
        options: ForkOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        self.ctx.host.assert_active()?;
        self.command_actions.fork(entry_id, options).await
    }

    /// Navigate to a different point in the session tree.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        self.ctx.host.assert_active()?;
        self.command_actions.navigate_tree(target_id, options).await
    }

    /// Switch to a different session file.
    pub async fn switch_session(
        &self,
        session_path: &str,
        options: SwitchSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        self.ctx.host.assert_active()?;
        self.command_actions
            .switch_session(session_path, options)
            .await
    }

    /// Reload extensions, skills, prompts, themes, and context files.
    pub async fn reload(&self) -> anyhow::Result<()> {
        self.ctx.host.assert_active()?;
        self.command_actions.reload().await
    }
}

/// Message accepted by `sendMessage()`: `Pick<CustomMessage, "customType" | "content" | "display" | "details">`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageInput {
    pub custom_type: String,
    pub content: UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// How a message is delivered while the agent streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliverAs {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
    #[serde(rename = "nextTurn")]
    NextTurn,
}

/// How a user message is delivered while the agent streams (`"steer" | "followUp"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

/// Options of `sendMessage()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<DeliverAs>,
}

/// Options of `sendUserMessage()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendUserMessageOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_as: Option<StreamingBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand_prompt_templates: Option<bool>,
}

/// Messaging actions of a [`ReplacedSessionContext`] (bound to the replacement session).
#[async_trait::async_trait]
pub trait ReplacedSessionActions: Send + Sync {
    async fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()>;
    async fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()>;
}

/// Fresh command-capable context bound to the replacement session after a session switch.
///
/// Passed to `with_session` callbacks of `new_session()`, `fork()`, and `switch_session()`.
#[derive(Clone)]
pub struct ReplacedSessionContext {
    pub cmd: ExtensionCommandContext,
    pub messaging: Arc<dyn ReplacedSessionActions>,
}

impl Deref for ReplacedSessionContext {
    type Target = ExtensionCommandContext;
    fn deref(&self) -> &ExtensionCommandContext {
        &self.cmd
    }
}

impl ReplacedSessionContext {
    pub async fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()> {
        self.cmd.ctx.host.assert_active()?;
        self.messaging.send_message(message, options).await
    }

    pub async fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()> {
        self.cmd.ctx.host.assert_active()?;
        self.messaging.send_user_message(content, options).await
    }
}

// ============================================================================
// Tool Types
// ============================================================================

/// Rendering options for tool results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRenderResultOptions {
    /// Whether the result view is expanded.
    pub expanded: bool,
    /// Whether this is a partial/streaming result.
    pub is_partial: bool,
}

/// Shared renderer state for one tool row (TS `TState`). Rust renderers store their own state
/// type and downcast; the JS bridge stores a handle to the JS state object.
pub type ToolRenderState = Arc<Mutex<Option<Box<dyn Any + Send + Sync>>>>;

/// Context passed to tool renderers.
#[derive(Clone)]
pub struct ToolRenderContext {
    /// Current tool call arguments. Shared across call/result renders for the same tool call.
    pub args: Value,
    /// Unique id for this tool execution. Stable across call/result renders for the same tool call.
    pub tool_call_id: String,
    /// Invalidate just this tool execution component for redraw.
    pub invalidate: Arc<dyn Fn() + Send + Sync>,
    /// Previously returned component for this render slot, if any.
    pub last_component: Option<ComponentRef>,
    /// Shared renderer state for this tool row. Initialized by tool-execution.
    pub state: ToolRenderState,
    /// Working directory for this tool execution.
    pub cwd: String,
    /// Whether the tool execution has started.
    pub execution_started: bool,
    /// Whether the tool call arguments are complete.
    pub args_complete: bool,
    /// Whether the tool result is partial/streaming.
    pub is_partial: bool,
    /// Whether the result view is expanded.
    pub expanded: bool,
    /// Whether inline images are currently shown in the TUI.
    pub show_images: bool,
    /// Whether the current result is an error.
    pub is_error: bool,
    /// Milliseconds the tool's execution took, from the final result; `None` while it runs, when
    /// it did not run, or for results stored before durations were recorded.
    pub duration_ms: Option<u64>,
    /// Horizontal padding configured by the outputPad setting. Renderers with
    /// `render_shell: Self_` apply it themselves.
    pub output_pad: usize,
}

/// How the model reaches a tool. "Callable" means callable from other tools through
/// `ctx.execute_tool()`, as the `codemode` tool does.
///
/// - `direct`: declared to the model while active, and callable while active.
/// - `model-only`: declared to the model while active, never callable.
/// - `codemode`: callable whenever registered. Not declared to the model unless explicitly activated.
/// - `deferred`: like `codemode`, but codemode tools do not list it; tool search can find it.
/// - `hidden`: registered but unreachable. Activating it has no effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolExposure {
    #[default]
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "model-only")]
    ModelOnly,
    #[serde(rename = "codemode")]
    Codemode,
    #[serde(rename = "deferred")]
    Deferred,
    #[serde(rename = "hidden")]
    Hidden,
}

impl ToolExposure {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ModelOnly => "model-only",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }
}

/// Hints about what a tool does, with the meaning of MCP tool annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// The tool does not modify its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may delete or overwrite data, rather than only add to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeating a call with the same arguments has no further effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool reaches an open world of external entities, such as the web.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// A group of related tools, such as the tools of one MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNamespace {
    /// For example `mcp__docs`.
    pub name: String,
    /// Other names that refer to the namespace where tools look it up by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Short summary shown once with the group in model-facing tool listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer usage guidance, such as MCP server instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The tools of a session as [`ToolDefinition::prepare_loadout`] sees them.
pub trait ToolLoadout: Send + Sync {
    /// Tools declared to the model (the active tools), in order, with their original descriptions.
    fn declared(&self) -> Vec<AgentToolRef>;
    /// Tools callable through `ctx.execute_tool()`.
    fn callable(&self) -> Vec<AgentToolRef>;
    /// Every registered tool.
    fn registered(&self) -> Vec<AgentToolRef>;
    fn get_exposure(&self, name: &str) -> ToolExposure;
    fn get_namespace(&self, name: &str) -> Option<ToolNamespace>;
    /// A tool's `prompt_guidelines`. Hidden declarations leave them out of the system prompt.
    fn get_prompt_guidelines(&self, name: &str) -> Vec<String>;
}

/// Changes [`ToolDefinition::prepare_loadout`] makes to what the model sees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLoadoutChanges {
    /// Model-facing descriptions of declared tools, by tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptions: Option<IndexMap<String, String>>,
    /// Declared tools whose declarations requests leave out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_declarations: Option<Vec<String>>,
}

/// Always `"all"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllTools {
    #[serde(rename = "all")]
    All,
}

/// Result of `reachesTools`: `"all"`, or the words of the input that may name tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReachesTools {
    All(AllTools),
    Words(Vec<String>),
}

/// Whether the tool execution component draws the standard shell or the tool draws its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RenderShell {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "self")]
    SelfRendered,
}

/// `execute(toolCallId, params, signal, onUpdate, ctx)`. `ctx` is `None` for tools wrapped without
/// a context factory (e.g. a built-in tool run in a plain `Agent`).
pub type ToolExecuteFn = Arc<
    dyn Fn(
            String,
            Value,
            Option<CancellationToken>,
            Option<AgentToolUpdateCallback>,
            Option<ExtensionToolContext>,
        ) -> BoxFuture<'static, anyhow::Result<AgentToolResult>>
        + Send
        + Sync,
>;
/// What an extension renderer returns when its code failed (TS: the renderer threw). Callers with
/// a fallback (tool calls and results, custom messages and entries) treat it like TS's `catch`;
/// elsewhere it shows the error text.
pub struct RenderFailed {
    pub message: String,
}

impl pi_tui::tui::Component for RenderFailed {
    fn render(&self, width: usize) -> Vec<String> {
        let text = format!("[extension render error: {}]", self.message);
        vec![pi_tui::utils::truncate_to_width(&text, width, "...", false)]
    }
}

/// The error message when `component` is a [`RenderFailed`].
pub fn render_failure(component: &ComponentRef) -> Option<&str> {
    component.as_any().downcast_ref::<RenderFailed>().map(|f| f.message.as_str())
}

/// `renderCall(args, theme, context)`.
pub type ToolRenderCallFn =
    Arc<dyn Fn(&Value, &Arc<Theme>, &ToolRenderContext) -> ComponentRef + Send + Sync>;
/// `renderResult(result, options, theme, context)`.
pub type ToolRenderResultFn = Arc<
    dyn Fn(
            &AgentToolResult,
            ToolRenderResultOptions,
            &Arc<Theme>,
            &ToolRenderContext,
        ) -> ComponentRef
        + Send
        + Sync,
>;
/// `prepareArguments(args)`: compatibility shim run before schema validation.
pub type PrepareArgumentsFn = Arc<dyn Fn(Value) -> Value + Send + Sync>;
/// `reachesTools(input)`.
pub type ReachesToolsFn = Arc<dyn Fn(&JsonObject) -> ReachesTools + Send + Sync>;
/// `prepareLoadout(loadout)`.
pub type PrepareLoadoutFn =
    Arc<dyn Fn(&dyn ToolLoadout) -> Option<ToolLoadoutChanges> + Send + Sync>;

/// Tool definition for `register_tool()`.
#[derive(Clone)]
pub struct ToolDefinition {
    /// Tool name (used in LLM tool calls).
    pub name: String,
    /// Human-readable label for UI.
    pub label: String,
    /// Description for LLM.
    pub description: String,
    /// One-line snippet for the Available tools section in the default system prompt. Custom
    /// tools are omitted from that section when this is not provided.
    pub prompt_snippet: Option<String>,
    /// Guideline bullets appended to the default system prompt Guidelines section when active.
    pub prompt_guidelines: Option<Vec<String>>,
    /// Parameter JSON schema (TypeBox in TS).
    pub parameters: Value,
    /// `parameters` was built with TypeBox (TS: the schema carries the non-enumerable `~kind`), so
    /// argument validation runs TypeBox's `Value.Convert` before the JSON Schema coercion. Set for
    /// the built-in tools and for JS extension tools whose schema comes from TypeBox; plain JSON
    /// schemas (MCP tools) leave it unset.
    pub parameters_typebox: bool,
    /// Provider-side constrained sampling request. TS `false` is `None`.
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
    /// Whether the tool execution component renders the standard shell or the tool its own framing.
    pub render_shell: Option<RenderShell>,
    /// Compatibility shim to prepare raw tool call arguments before schema validation.
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    /// JSON Schema of `structured_content` in successful results.
    pub output_schema: Option<Value>,
    /// How the model reaches the tool. Default: `direct`.
    pub exposure: Option<ToolExposure>,
    /// Group the tool belongs to, for example its MCP server.
    pub namespace: Option<ToolNamespace>,
    /// For tools that reach other tools: which tools a call with `input` may reach.
    pub reaches_tools: Option<ReachesToolsFn>,
    /// Hints about what the tool does.
    pub annotations: Option<ToolAnnotations>,
    /// Whether registering the tool activates it. Default: true for `direct` and `model-only`.
    pub default_active: Option<bool>,
    /// Adjust how the loadout is presented to the model while this tool is active.
    pub prepare_loadout: Option<PrepareLoadoutFn>,
    /// Per-tool execution mode override.
    pub execution_mode: Option<ToolExecutionMode>,
    /// Execute the tool.
    pub execute: ToolExecuteFn,
    /// Custom rendering for tool call display.
    pub render_call: Option<ToolRenderCallFn>,
    /// Custom rendering for tool result display.
    pub render_result: Option<ToolRenderResultFn>,
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("exposure", &self.exposure)
            .finish_non_exhaustive()
    }
}

impl ToolDefinition {
    /// A definition with the required fields; everything optional unset.
    pub fn new<F, Fut>(
        name: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: F,
    ) -> Self
    where
        F: Fn(
                String,
                Value,
                Option<CancellationToken>,
                Option<AgentToolUpdateCallback>,
                Option<ExtensionToolContext>,
            ) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = anyhow::Result<AgentToolResult>> + Send + 'static,
    {
        Self {
            name: name.into(),
            label: label.into(),
            description: description.into(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters,
            parameters_typebox: false,
            constrained_sampling: None,
            render_shell: None,
            prepare_arguments: None,
            output_schema: None,
            exposure: None,
            namespace: None,
            reaches_tools: None,
            annotations: None,
            default_active: None,
            prepare_loadout: None,
            execution_mode: None,
            execute: Arc::new(move |id, params, signal, on_update, ctx| {
                Box::pin(execute(id, params, signal, on_update, ctx))
            }),
            render_call: None,
            render_result: None,
        }
    }

    /// The effective exposure (`direct` when unset).
    pub fn effective_exposure(&self) -> ToolExposure {
        self.exposure.unwrap_or_default()
    }

    /// The renderer-related fields (`Pick<ToolDefinition, "renderShell" | "renderCall" | "renderResult">`).
    pub fn renderers(&self) -> ToolRenderers {
        ToolRenderers {
            render_shell: self.render_shell,
            render_call: self.render_call.clone(),
            render_result: self.render_result.clone(),
        }
    }
}

/// Preserve parameter inference for standalone tool definitions (identity in Rust).
pub fn define_tool(tool: ToolDefinition) -> ToolDefinition {
    tool
}

/// `Pick<ToolDefinition, "renderShell" | "renderCall" | "renderResult">`.
#[derive(Clone, Default)]
pub struct ToolRenderers {
    pub render_shell: Option<RenderShell>,
    pub render_call: Option<ToolRenderCallFn>,
    pub render_result: Option<ToolRenderResultFn>,
}

/// Chooses how calls to a tool are drawn, including tools that are not registered. `next()`
/// returns the renderers the remaining resolvers, then the registered tool, would use.
pub type ToolRendererResolver =
    Arc<dyn Fn(&str, &dyn Fn() -> Option<ToolRenderers>) -> Option<ToolRenderers> + Send + Sync>;

// ============================================================================
// Startup/Resource Events
// ============================================================================

/// `project_trust`: decide whether project-local resources are trusted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEvent {
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrustEventDecision {
    Yes,
    No,
    Undecided,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustEventResult {
    pub trusted: ProjectTrustEventDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
}

/// Context of `project_trust` handlers (`ui` is limited to select/confirm/input/notify in TS).
#[derive(Clone)]
pub struct ProjectTrustContext {
    pub cwd: String,
    pub mode: ExtensionMode,
    pub has_ui: bool,
    pub ui: Arc<dyn ExtensionUIContext>,
}

/// Handler of `project_trust` (registered with [`ExtensionAPI::on_project_trust`]).
pub type ProjectTrustHandler = Arc<
    dyn Fn(
            ProjectTrustEvent,
            ProjectTrustContext,
        ) -> BoxFuture<'static, anyhow::Result<ProjectTrustEventResult>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourcesDiscoverReason {
    Startup,
    Reload,
}

/// Fired after session_start to allow extensions to provide additional resource paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverEvent {
    pub cwd: String,
    pub reason: ResourcesDiscoverReason,
}

/// Result from resources_discover event handler.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesDiscoverResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_paths: Option<Vec<String>>,
}

/// Fired when an extension registers or unregisters an MCP server after the extensions are bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersChangeEvent {
    /// Every registered server after the change.
    pub servers: Vec<RegisteredMcpServer>,
}

// ============================================================================
// Session Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStartReason {
    Startup,
    Reload,
    New,
    Resume,
    Fork,
}

/// Fired when a session is started, loaded, or reloaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartEvent {
    /// Why this session start happened.
    pub reason: SessionStartReason,
    /// Previously active session file. Present for "new", "resume", and "fork".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session_file: Option<String>,
}

/// Fired when the current session metadata changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoChangedEvent {
    /// Current normalized session name. `None` when the name is cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSwitchReason {
    New,
    Resume,
}

/// Fired before switching to another session (can be cancelled).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchEvent {
    pub reason: SessionSwitchReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// Fired before forking a session (can be cancelled).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkEvent {
    pub entry_id: String,
    pub position: ForkPosition,
}

/// What triggered a compaction: manual /compact, the context threshold, or overflow recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Fired before context compaction (can be cancelled or customized).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactEvent {
    pub preparation: CompactionPreparation,
    pub branch_entries: Vec<SessionEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    pub reason: CompactionReason,
    /// True when the aborted turn is retried after this compaction (overflow recovery).
    pub will_retry: bool,
    /// Not serialized; the JS bridge passes it as an `AbortSignal`.
    #[serde(skip)]
    pub signal: CancellationToken,
}

/// Fired after context compaction succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactEvent {
    pub compaction_entry: CompactionEntry,
    pub from_extension: bool,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

/// Fired after context compaction fails or is aborted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactFailedEvent {
    pub reason: CompactionReason,
    /// Error text when compaction failed for a non-abort reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// True when compaction was cancelled or aborted.
    pub aborted: bool,
    pub will_retry: bool,
    /// True when the failing compaction content came from a session_before_compact handler.
    pub from_extension: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionShutdownReason {
    Quit,
    Reload,
    New,
    Resume,
    Fork,
}

/// Fired before an extension runtime is torn down due to quit, reload, or session replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionShutdownEvent {
    pub reason: SessionShutdownReason,
    /// Destination session file when shutting down due to session replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_file: Option<String>,
}

/// Preparation data for tree navigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreePreparation {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    pub common_ancestor_id: Option<String>,
    pub entries_to_summarize: Vec<SessionEntry>,
    pub user_wants_summary: bool,
    /// Custom instructions for summarization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    /// If true, custom_instructions replaces the default prompt instead of being appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    /// Label to attach to the branch summary entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Fired before navigating in the session tree (can be cancelled).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent {
    pub preparation: TreePreparation,
    #[serde(skip)]
    pub signal: CancellationToken,
}

/// Fired after navigating in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEvent {
    pub new_leaf_id: Option<String>,
    pub old_leaf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_entry: Option<BranchSummaryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_extension: Option<bool>,
}

/// The session events (`SessionEvent` union), tagged by `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "session_start")]
    SessionStart(SessionStartEvent),
    #[serde(rename = "session_info_changed")]
    SessionInfoChanged(SessionInfoChangedEvent),
    #[serde(rename = "session_before_switch")]
    SessionBeforeSwitch(SessionBeforeSwitchEvent),
    #[serde(rename = "session_before_fork")]
    SessionBeforeFork(SessionBeforeForkEvent),
    #[serde(rename = "session_before_compact")]
    SessionBeforeCompact(SessionBeforeCompactEvent),
    #[serde(rename = "session_compact")]
    SessionCompact(SessionCompactEvent),
    #[serde(rename = "session_compact_failed")]
    SessionCompactFailed(SessionCompactFailedEvent),
    #[serde(rename = "session_shutdown")]
    SessionShutdown(SessionShutdownEvent),
    #[serde(rename = "session_before_tree")]
    SessionBeforeTree(SessionBeforeTreeEvent),
    #[serde(rename = "session_tree")]
    SessionTree(SessionTreeEvent),
}

// ============================================================================
// Agent Events
// ============================================================================

/// Fired before each LLM call. Can modify messages.
///
/// `messages` holds the conversation without system messages. Pi restores the prompt and tool
/// state after the handler returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEvent {
    pub messages: Vec<AgentMessage>,
}

/// Fired before each LLM call, after every `context` handler has run. `messages` is the full
/// transcript including system messages, and the result is sent as returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextWithSystemEvent {
    pub messages: Vec<AgentMessage>,
}

/// Fired before a provider request is sent. Can replace the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderRequestEvent {
    pub payload: Value,
}

/// Fired after request headers are assembled, before the provider HTTP call.
/// In TS handlers mutate `headers` in place; in Rust they return
/// [`BeforeProviderHeadersEventResult`]. A `None` (null) value deletes that header.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderHeadersEvent {
    pub headers: ProviderHeaders,
}

/// Fired after a provider response is received and before the response stream is consumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterProviderResponseEvent {
    pub status: u16,
    pub headers: IndexMap<String, String>,
}

/// Fired for a parsed provider stream event before Pi normalizes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStreamEvent {
    pub provider: ProviderId,
    pub api: Api,
    pub model: String,
    pub data: Value,
}

/// What started a run: a user prompt or a custom message (`messageType` discriminant).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "messageType", rename_all_fields = "camelCase")]
pub enum BeforeAgentStartKind {
    /// A run started by a submitted prompt, RPC prompt, or `pi.sendUserMessage()`.
    #[serde(rename = "user")]
    User {
        /// The raw user prompt text (after expansion).
        prompt: String,
        /// Images attached to the user prompt, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    /// A run started by `pi.sendMessage(..., { triggerTurn: true })` while idle.
    #[serde(rename = "custom")]
    Custom {
        /// The custom message that starts the run.
        message: CustomMessage,
    },
}

/// Fired before a run starts from a user message or an idle custom message with `triggerTurn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEvent {
    /// The current system prompt, rendered from system_prompt_options and earlier handler changes.
    pub system_prompt: String,
    /// Prompt sections. Later handlers observe changes returned by earlier handlers.
    pub system_prompt_options: NormalizedBuildSystemPromptOptions,
    #[serde(flatten)]
    pub kind: BeforeAgentStartKind,
}

/// Alias of the user variant (TS `BeforeAgentStartUserEvent`).
pub type BeforeAgentStartUserEvent = BeforeAgentStartEvent;
/// Alias of the custom variant (TS `BeforeAgentStartCustomEvent`).
pub type BeforeAgentStartCustomEvent = BeforeAgentStartEvent;

/// Fired when an agent loop starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStartEvent {}

/// Fired when an agent loop ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEndEvent {
    pub messages: Vec<AgentMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentActivityOutcome {
    Completed,
    Aborted,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntryDraft {
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntryDraft {
    pub custom_type: String,
    pub content: UserContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntryDraft {
    pub target_id: String,
    /// `None` (null) omits the target entry from the context.
    pub replacement: Option<ContextEditEntryReplacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntryDraft {
    pub summary: String,
    /// `None` (null) creates a self-retaining compaction that keeps no preceding entries.
    pub first_kept_entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// An entry a boundary handler appends, tagged by `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionBoundaryDraft {
    #[serde(rename = "custom")]
    Custom(CustomEntryDraft),
    #[serde(rename = "custom_message")]
    CustomMessage(CustomMessageEntryDraft),
    #[serde(rename = "context_edit")]
    ContextEdit(ContextEditEntryDraft),
    #[serde(rename = "compaction")]
    Compaction(CompactionEntryDraft),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryContextPreview {
    pub context_entries: Vec<ProjectedSessionEntry>,
    pub context_messages: Vec<AgentMessage>,
    pub llm_messages: Vec<Message>,
    pub pending_messages: Vec<AgentMessage>,
    pub can_continue: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryState {
    pub entries: Vec<SessionBoundaryDraft>,
    #[serde(rename = "continue")]
    pub continue_: bool,
    pub context: BoundaryContextPreview,
    pub outcome: AgentActivityOutcome,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<SessionBoundaryDraft>>,
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
}

/// Fired before final settlement. May append entries and ensure one next provider request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBeforeSettleEvent {
    #[serde(flatten)]
    pub state: BoundaryState,
}

/// Fired after an agent run has fully settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettledEvent {
    /// Whether the run ended because it was aborted, for example with Escape.
    pub aborted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UIPromptKind {
    Select,
    Confirm,
    Input,
    Editor,
    Custom,
}

/// Always `"ui_prompt"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UIPromptReason {
    #[default]
    #[serde(rename = "ui_prompt")]
    UiPrompt,
}

/// Fired when Pi starts waiting on a blocking user-facing extension UI prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UIPromptStartEvent {
    pub reason: UIPromptReason,
    pub kind: UIPromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Fired when Pi is no longer waiting on a blocking user-facing extension UI prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UIPromptEndEvent {
    pub reason: UIPromptReason,
    pub kind: UIPromptKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Fired at the start of each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartEvent {
    pub turn_index: u64,
    pub timestamp: i64,
}

/// Fired at the end of each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndEvent {
    #[serde(flatten)]
    pub state: BoundaryState,
    pub turn_index: u64,
    pub message: AgentMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub message_entry_id: String,
    pub tool_result_entry_ids: Vec<String>,
}

/// Fired when a message starts (user, assistant, or toolResult).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageStartEvent {
    pub message: AgentMessage,
}

/// Fired during assistant message streaming with token-by-token updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdateEvent {
    pub message: AgentMessage,
    pub assistant_message_event: AssistantMessageEvent,
}

/// Fired when a message ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEndEvent {
    pub message: AgentMessage,
}

/// Fired when a tool starts executing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStartEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    /// Set when another tool (for example a codemode script) made this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// Fired during tool execution with partial/streaming output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdateEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub partial_result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

/// Fired when a tool finishes executing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEndEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Value,
    pub is_error: bool,
    /// Milliseconds `execute()` took; absent when the tool did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

// ============================================================================
// Model Events
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSelectSource {
    Set,
    Cycle,
    Restore,
}

/// Fired when a new model is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelectEvent {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_model: Option<Model>,
    pub source: ModelSelectSource,
}

/// Fired when a new thinking level is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelSelectEvent {
    pub level: ThinkingLevel,
    pub previous_level: ThinkingLevel,
}

// ============================================================================
// User Bash / Input Events
// ============================================================================

/// Fired when user executes a bash command via ! or !! prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserBashEvent {
    /// The command to execute.
    pub command: String,
    /// True if !! prefix was used (excluded from LLM context).
    pub exclude_from_context: bool,
    /// Current working directory.
    pub cwd: String,
}

/// Source of user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputSource {
    Interactive,
    Rpc,
    Extension,
}

/// Fired when user input is received, before agent processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputEvent {
    /// The input text.
    pub text: String,
    /// Attached images, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    /// Where the input came from.
    pub source: InputSource,
    /// How the input will be delivered during streaming, or `None` when idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_behavior: Option<StreamingBehavior>,
}

/// Result from input event handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all_fields = "camelCase")]
pub enum InputEventResult {
    #[serde(rename = "continue")]
    Continue,
    #[serde(rename = "transform")]
    Transform {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    #[serde(rename = "handled")]
    Handled,
}

// ============================================================================
// Tool Events
// ============================================================================

/// Fired before a tool executes. Can block, or replace the arguments through
/// [`ToolCallEventResult::input`] (TS: mutate `event.input` in place). Later handlers see earlier
/// replacements. No re-validation is performed.
///
/// The TS union of per-tool events (`BashToolCallEvent`, ...) is this one struct; use
/// [`ToolCallEvent::typed`] or the `*ToolCallEvent` aliases for typed inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvent {
    /// The call's id. Nested calls (with `parent_tool_call_id`) get `<parent id>/<n>`.
    pub tool_call_id: String,
    /// Set when another tool (for example a codemode script) issued this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub tool_name: String,
    pub input: JsonObject,
}

/// A [`ToolCallEvent`] with typed input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypedToolCallEvent<I> {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub tool_name: String,
    pub input: I,
}

pub type BashToolCallEvent = TypedToolCallEvent<BashToolInput>;
pub type PowerShellToolCallEvent = TypedToolCallEvent<PowerShellToolInput>;
pub type ReadToolCallEvent = TypedToolCallEvent<ReadToolInput>;
pub type EditToolCallEvent = TypedToolCallEvent<EditToolInput>;
pub type WriteToolCallEvent = TypedToolCallEvent<WriteToolInput>;
pub type GrepToolCallEvent = TypedToolCallEvent<GrepToolInput>;
pub type FindToolCallEvent = TypedToolCallEvent<FindToolInput>;
pub type LsToolCallEvent = TypedToolCallEvent<LsToolInput>;
pub type CustomToolCallEvent = ToolCallEvent;

impl ToolCallEvent {
    /// The event with its input parsed as `I`.
    pub fn typed<I: DeserializeOwned>(&self) -> anyhow::Result<TypedToolCallEvent<I>> {
        Ok(TypedToolCallEvent {
            tool_call_id: self.tool_call_id.clone(),
            parent_tool_call_id: self.parent_tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            input: serde_json::from_value(Value::Object(self.input.clone()))?,
        })
    }
}

/// Type guard for narrowing a [`ToolCallEvent`] by tool name.
pub fn is_tool_call_event_type(tool_name: &str, event: &ToolCallEvent) -> bool {
    event.tool_name == tool_name
}

/// Fired after a tool executes. Can modify the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    /// The call's id; `<parent id>/<n>` for nested calls.
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub tool_name: String,
    pub input: JsonObject,
    pub content: Vec<UserContentBlock>,
    /// Machine-readable result for tools that declare an `output_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    pub is_error: bool,
    /// Usage from the tool execution itself, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Tool-specific details (`BashToolDetails`, ...); `None` when undefined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// A [`ToolResultEvent`] with typed details.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypedToolResultEvent<D> {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub tool_name: String,
    pub input: JsonObject,
    pub content: Vec<UserContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<D>,
}

pub type BashToolResultEvent = TypedToolResultEvent<BashToolDetails>;
pub type PowerShellToolResultEvent = TypedToolResultEvent<PowerShellToolDetails>;
pub type ReadToolResultEvent = TypedToolResultEvent<ReadToolDetails>;
pub type EditToolResultEvent = TypedToolResultEvent<EditToolDetails>;
pub type WriteToolResultEvent = TypedToolResultEvent<Value>;
pub type GrepToolResultEvent = TypedToolResultEvent<GrepToolDetails>;
pub type FindToolResultEvent = TypedToolResultEvent<FindToolDetails>;
pub type LsToolResultEvent = TypedToolResultEvent<LsToolDetails>;
pub type CustomToolResultEvent = ToolResultEvent;

impl ToolResultEvent {
    /// The event with its details parsed as `D`.
    pub fn typed<D: DeserializeOwned>(&self) -> anyhow::Result<TypedToolResultEvent<D>> {
        Ok(TypedToolResultEvent {
            tool_call_id: self.tool_call_id.clone(),
            parent_tool_call_id: self.parent_tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            input: self.input.clone(),
            content: self.content.clone(),
            structured_content: self.structured_content.clone(),
            is_error: self.is_error,
            usage: self.usage.clone(),
            details: match &self.details {
                Some(d) => Some(serde_json::from_value(d.clone())?),
                None => None,
            },
        })
    }
}

pub fn is_bash_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "bash"
}
pub fn is_power_shell_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "powershell"
}
pub fn is_read_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "read"
}
pub fn is_edit_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "edit"
}
pub fn is_write_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "write"
}
pub fn is_grep_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "grep"
}
pub fn is_find_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "find"
}
pub fn is_ls_tool_result(e: &ToolResultEvent) -> bool {
    e.tool_name == "ls"
}

// ============================================================================
// Event Results
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEventResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<AgentMessage>>,
}

pub type TurnEndEventResult = BoundaryResult;
pub type AgentBeforeSettleEventResult = BoundaryResult;

/// Replacement provider payload (`unknown` in TS). Returning nothing keeps the payload.
pub type BeforeProviderRequestEventResult = Value;

/// Rust form of the in-place header mutation of `before_provider_headers`: the complete header
/// map after the handler's changes. A `None` value deletes that header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeProviderHeadersEventResult {
    pub headers: ProviderHeaders,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEventResult {
    /// Block tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Hint that the agent should stop after the current tool batch when this call is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    /// Rust form of mutating `event.input` in place: the replacement arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<JsonObject>,
}

/// Result from user_bash event handler.
#[derive(Clone)]
pub enum UserBashEventResult {
    /// Custom operations to use for execution.
    Operations(BashOperations),
    /// Full replacement: extension handled execution, use this result.
    Result(BashResult),
}

/// Changes a `tool_result` handler makes. Omitted fields stay as they are, except that replacing
/// `content` without returning `structured_content` drops the structured content.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<UserContentBlock>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEndEventResult {
    /// Replace the finalized message. The replacement must keep the original message role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<AgentMessage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEventResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<CustomMessageInput>,
    /// Replace the complete system prompt for this turn. Later handlers observe this exact override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Rust form of mutating `event.systemPromptOptions` in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_options: Option<NormalizedBuildSystemPromptOptions>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeSwitchResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_conversation_restore: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionResult>,
}

/// Summary supplied by a `session_before_tree` handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeSummary {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionBeforeTreeSummary>,
    /// Override custom instructions for summarization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    /// Override whether custom_instructions replaces the default prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    /// Override label to attach to the branch summary entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ============================================================================
// The event enum, handler results, typed registration
// ============================================================================

/// Generates [`ExtensionEvent`], [`ExtensionEventResult`] and the [`ExtensionEventPayload`] impls.
macro_rules! extension_events {
    (
        events { $( $variant:ident($event:ty) = $name:literal => $result:ty ),* $(,)? }
        results { $( $rvariant:ident($rty:ty) ),* $(,)? }
    ) => {
        /// Union of all event types, tagged by `type` exactly like the TS `ExtensionEvent`.
        ///
        /// `project_trust` is included for completeness, but its handlers are registered with
        /// [`ExtensionAPI::on_project_trust`] because they take a [`ProjectTrustContext`].
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type")]
        // Mirrors a TS union; boxing the large variants would add indirection to every
        // construction and match across the crate for little memory saved.
        #[allow(clippy::large_enum_variant)]
        pub enum ExtensionEvent {
            $( #[serde(rename = $name)] $variant($event), )*
        }

        impl ExtensionEvent {
            /// The TS `type` discriminant.
            pub fn type_name(&self) -> &'static str {
                match self { $( Self::$variant(_) => $name, )* }
            }
        }

        /// Every event type name, in declaration order.
        pub const EXTENSION_EVENT_TYPES: &[&str] = &[ $( $name ),* ];

        $(
            impl ExtensionEventPayload for $event {
                const TYPE: &'static str = $name;
                type Result = $result;
                fn from_event(event: ExtensionEvent) -> Option<Self> {
                    match event { ExtensionEvent::$variant(e) => Some(e), _ => None }
                }
                fn into_event(self) -> ExtensionEvent {
                    ExtensionEvent::$variant(self)
                }
            }
        )*

        /// Value returned by a handler. Rust handlers return a typed variant; JS handlers return
        /// [`ExtensionEventResult::Json`], converted by [`EventResultType::from_result`].
        #[derive(Clone)]
        pub enum ExtensionEventResult {
            $( $rvariant($rty), )*
            UserBash(UserBashEventResult),
            /// Untyped result (e.g. from a JS handler).
            Json(Value),
        }

        $(
            impl From<$rty> for ExtensionEventResult {
                fn from(value: $rty) -> Self { Self::$rvariant(value) }
            }
            impl EventResultType for $rty {
                fn from_result(result: ExtensionEventResult) -> anyhow::Result<Option<Self>> {
                    match result {
                        ExtensionEventResult::$rvariant(v) => Ok(Some(v)),
                        ExtensionEventResult::Json(Value::Null) => Ok(None),
                        ExtensionEventResult::Json(v) => Ok(Some(serde_json::from_value(v)?)),
                        _ => anyhow::bail!(concat!("handler returned a result that is not ", stringify!($rty))),
                    }
                }
            }
            impl IntoEventResult for $rty {
                fn into_event_result(self) -> Option<ExtensionEventResult> { Some(self.into()) }
            }
        )*
    };
}

extension_events! {
    events {
        ProjectTrust(ProjectTrustEvent) = "project_trust" => ProjectTrustEventResult,
        ResourcesDiscover(ResourcesDiscoverEvent) = "resources_discover" => ResourcesDiscoverResult,
        McpServersChange(McpServersChangeEvent) = "mcp_servers_change" => (),
        SessionStart(SessionStartEvent) = "session_start" => (),
        SessionInfoChanged(SessionInfoChangedEvent) = "session_info_changed" => (),
        SessionBeforeSwitch(SessionBeforeSwitchEvent) = "session_before_switch" => SessionBeforeSwitchResult,
        SessionBeforeFork(SessionBeforeForkEvent) = "session_before_fork" => SessionBeforeForkResult,
        SessionBeforeCompact(SessionBeforeCompactEvent) = "session_before_compact" => SessionBeforeCompactResult,
        SessionCompact(SessionCompactEvent) = "session_compact" => (),
        SessionCompactFailed(SessionCompactFailedEvent) = "session_compact_failed" => (),
        SessionShutdown(SessionShutdownEvent) = "session_shutdown" => (),
        SessionBeforeTree(SessionBeforeTreeEvent) = "session_before_tree" => SessionBeforeTreeResult,
        SessionTree(SessionTreeEvent) = "session_tree" => (),
        Context(ContextEvent) = "context" => ContextEventResult,
        ContextWithSystem(ContextWithSystemEvent) = "context_with_system" => ContextEventResult,
        CacheWarmingDecision(CacheWarmingDecisionEvent) = "cache_warming_decision" => CacheWarmingDecisionEventResult,
        BeforeProviderRequest(BeforeProviderRequestEvent) = "before_provider_request" => Value,
        BeforeProviderHeaders(BeforeProviderHeadersEvent) = "before_provider_headers" => BeforeProviderHeadersEventResult,
        AfterProviderResponse(AfterProviderResponseEvent) = "after_provider_response" => (),
        ProviderStream(ProviderStreamEvent) = "provider_stream_event" => (),
        BeforeAgentStart(BeforeAgentStartEvent) = "before_agent_start" => BeforeAgentStartEventResult,
        AgentStart(AgentStartEvent) = "agent_start" => (),
        AgentEnd(AgentEndEvent) = "agent_end" => (),
        AgentBeforeSettle(AgentBeforeSettleEvent) = "agent_before_settle" => BoundaryResult,
        AgentSettled(AgentSettledEvent) = "agent_settled" => (),
        UIPromptStart(UIPromptStartEvent) = "ui_prompt_start" => (),
        UIPromptEnd(UIPromptEndEvent) = "ui_prompt_end" => (),
        TurnStart(TurnStartEvent) = "turn_start" => (),
        TurnEnd(TurnEndEvent) = "turn_end" => BoundaryResult,
        MessageStart(MessageStartEvent) = "message_start" => (),
        MessageUpdate(MessageUpdateEvent) = "message_update" => (),
        MessageEnd(MessageEndEvent) = "message_end" => MessageEndEventResult,
        ToolExecutionStart(ToolExecutionStartEvent) = "tool_execution_start" => (),
        ToolExecutionUpdate(ToolExecutionUpdateEvent) = "tool_execution_update" => (),
        ToolExecutionEnd(ToolExecutionEndEvent) = "tool_execution_end" => (),
        ModelSelect(ModelSelectEvent) = "model_select" => (),
        ThinkingLevelSelect(ThinkingLevelSelectEvent) = "thinking_level_select" => (),
        UserBash(UserBashEvent) = "user_bash" => UserBashEventResult,
        Input(InputEvent) = "input" => InputEventResult,
        ToolCall(ToolCallEvent) = "tool_call" => ToolCallEventResult,
        ToolResult(ToolResultEvent) = "tool_result" => ToolResultEventResult,
    }
    results {
        ProjectTrust(ProjectTrustEventResult),
        ResourcesDiscover(ResourcesDiscoverResult),
        SessionBeforeSwitch(SessionBeforeSwitchResult),
        SessionBeforeFork(SessionBeforeForkResult),
        SessionBeforeCompact(SessionBeforeCompactResult),
        SessionBeforeTree(SessionBeforeTreeResult),
        Context(ContextEventResult),
        CacheWarmingDecision(CacheWarmingDecisionEventResult),
        BeforeProviderRequest(Value),
        BeforeProviderHeaders(BeforeProviderHeadersEventResult),
        BeforeAgentStart(BeforeAgentStartEventResult),
        Boundary(BoundaryResult),
        MessageEnd(MessageEndEventResult),
        Input(InputEventResult),
        ToolCall(ToolCallEventResult),
        ToolResult(ToolResultEventResult),
    }
}

impl From<UserBashEventResult> for ExtensionEventResult {
    fn from(value: UserBashEventResult) -> Self {
        Self::UserBash(value)
    }
}

impl EventResultType for UserBashEventResult {
    fn from_result(result: ExtensionEventResult) -> anyhow::Result<Option<Self>> {
        match result {
            ExtensionEventResult::UserBash(v) => Ok(Some(v)),
            ExtensionEventResult::Json(Value::Null) => Ok(None),
            ExtensionEventResult::Json(_) => {
                anyhow::bail!(
                    "user_bash results with operations cannot be passed as JSON; the bridge must build them"
                )
            }
            _ => anyhow::bail!("handler returned a result that is not UserBashEventResult"),
        }
    }
}

impl IntoEventResult for UserBashEventResult {
    fn into_event_result(self) -> Option<ExtensionEventResult> {
        Some(self.into())
    }
}

impl IntoEventResult for () {
    fn into_event_result(self) -> Option<ExtensionEventResult> {
        None
    }
}

/// An event struct that is a variant of [`ExtensionEvent`].
pub trait ExtensionEventPayload: Sized + Send + 'static {
    /// The TS `type` discriminant.
    const TYPE: &'static str;
    /// What handlers of this event may return (`()` when the result is ignored).
    type Result: IntoEventResult + Send + 'static;
    fn from_event(event: ExtensionEvent) -> Option<Self>;
    fn into_event(self) -> ExtensionEvent;
}

/// Conversion of a handler's typed result into an [`ExtensionEventResult`].
pub trait IntoEventResult {
    fn into_event_result(self) -> Option<ExtensionEventResult>;
}

/// A typed handler result that can be recovered from an [`ExtensionEventResult`].
pub trait EventResultType: Sized {
    /// `Ok(None)` for "no result" (JS `undefined`/`null`).
    fn from_result(result: ExtensionEventResult) -> anyhow::Result<Option<Self>>;
}

/// Handler function type for events (TS `ExtensionHandler<E, R>`, type-erased).
pub type ExtensionHandler = Arc<
    dyn Fn(
            ExtensionEvent,
            ExtensionContext,
        ) -> BoxFuture<'static, anyhow::Result<Option<ExtensionEventResult>>>
        + Send
        + Sync,
>;

/// A registered handler as stored in [`Extension::handlers`].
#[derive(Clone)]
pub enum HandlerFn {
    Event(ExtensionHandler),
    ProjectTrust(ProjectTrustHandler),
}

/// Wrap a typed handler into an [`ExtensionHandler`].
pub fn typed_handler<E, F, Fut>(handler: F) -> ExtensionHandler
where
    E: ExtensionEventPayload,
    F: Fn(E, ExtensionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<Option<E::Result>>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |event: ExtensionEvent, ctx: ExtensionContext| {
        let handler = handler.clone();
        Box::pin(async move {
            let Some(event) = E::from_event(event) else {
                anyhow::bail!("handler for {} received a different event", E::TYPE);
            };
            Ok(handler(event, ctx)
                .await?
                .and_then(IntoEventResult::into_event_result))
        })
    })
}

/// Typed `pi.on(E::TYPE, handler)`.
pub fn on_event<E, F, Fut>(pi: &dyn ExtensionAPI, handler: F) -> anyhow::Result<Unsubscribe>
where
    E: ExtensionEventPayload,
    F: Fn(E, ExtensionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<Option<E::Result>>> + Send + 'static,
{
    pi.on(E::TYPE, typed_handler::<E, F, Fut>(handler))
}

// ============================================================================
// Message and Entry Rendering
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRenderOptions {
    pub expanded: bool,
    /// Horizontal padding configured by the outputPad setting.
    pub output_pad: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarkdownMessageType {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "assistant-thinking")]
    AssistantThinking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownTransformContext {
    pub message_type: MarkdownMessageType,
    pub is_streaming: bool,
    pub available_width: usize,
    /// Persisted session-entry id; absent for live messages or live rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Persisted session-entry ISO timestamp; absent for live messages or live rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

pub type MarkdownTransformer = Arc<dyn Fn(&str, &MarkdownTransformContext) -> String + Send + Sync>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRenderOptions {
    pub expanded: bool,
}

pub type MessageRenderer = Arc<
    dyn Fn(&CustomMessage, &MessageRenderOptions, &Arc<Theme>) -> Option<ComponentRef>
        + Send
        + Sync,
>;

pub type EntryRenderer = Arc<
    dyn Fn(&CustomEntry, &EntryRenderOptions, &Arc<Theme>) -> Option<ComponentRef> + Send + Sync,
>;

// ============================================================================
// Command Registration
// ============================================================================

/// `getArgumentCompletions(argumentPrefix)`.
pub type ArgumentCompletionsFn =
    Arc<dyn Fn(String) -> BoxFuture<'static, Option<Vec<AutocompleteItem>>> + Send + Sync>;

/// Command handler `(args, ctx)`.
pub type CommandHandler = Arc<
    dyn Fn(String, ExtensionCommandContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync,
>;

/// Options of `registerCommand()` (`Omit<RegisteredCommand, "name" | "sourceInfo">`).
#[derive(Clone)]
pub struct CommandOptions {
    pub description: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
    pub handler: CommandHandler,
}

#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub source_info: SourceInfo,
    pub description: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
    pub handler: CommandHandler,
}

#[derive(Clone)]
pub struct ResolvedCommand {
    pub command: RegisteredCommand,
    pub invocation_name: String,
}

impl Deref for ResolvedCommand {
    type Target = RegisteredCommand;
    fn deref(&self) -> &RegisteredCommand {
        &self.command
    }
}

// ============================================================================
// Extension API
// ============================================================================

/// Shortcut handler `(ctx)`.
pub type ShortcutHandler =
    Arc<dyn Fn(ExtensionContext) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Options of `registerShortcut()`.
#[derive(Clone)]
pub struct ShortcutOptions {
    pub description: Option<String>,
    pub handler: ShortcutHandler,
}

/// Type of a CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagType {
    Boolean,
    String,
}

/// Value of a CLI flag (`boolean | string`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    Bool(bool),
    String(String),
}

/// Options of `registerFlag()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub flag_type: FlagType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<FlagValue>,
}

/// Argument of `registerProvider()`: a native pi-ai provider or a legacy provider config.
#[derive(Clone)]
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub enum ProviderRegistration {
    Native(Arc<dyn Provider>),
    Config {
        name: String,
        config: ProviderConfig,
    },
}

/// The `pi` object passed to extension factory functions (TS `ExtensionAPI`).
///
/// Implemented in `loader.rs` (`createExtensionAPI`). Every method fails when the extension
/// instance is stale (after reload or session replacement).
#[async_trait::async_trait]
pub trait ExtensionAPI: Send + Sync {
    // ----- Event Subscription -----

    /// Subscribe to an event by its `type` name (see [`EXTENSION_EVENT_TYPES`] and [`on_event`]).
    fn on(&self, event: &str, handler: ExtensionHandler) -> anyhow::Result<Unsubscribe>;

    /// Subscribe to `project_trust`.
    fn on_project_trust(&self, handler: ProjectTrustHandler) -> anyhow::Result<Unsubscribe>;

    // ----- Tool Registration -----

    /// Register a tool that the LLM can call.
    fn register_tool(&self, tool: ToolDefinition) -> anyhow::Result<()>;

    // ----- Command, Shortcut, Flag Registration -----

    /// Register a custom command.
    fn register_command(&self, name: &str, options: CommandOptions) -> anyhow::Result<()>;

    /// Register a keyboard shortcut.
    fn register_shortcut(&self, shortcut: &str, options: ShortcutOptions) -> anyhow::Result<()>;

    /// Register a CLI flag.
    fn register_flag(&self, name: &str, options: FlagOptions) -> anyhow::Result<()>;

    /// Get the value of a registered CLI flag.
    fn get_flag(&self, name: &str) -> anyhow::Result<Option<FlagValue>>;

    // ----- Message Rendering -----

    /// Register a custom renderer for CustomMessageEntry.
    fn register_message_renderer(
        &self,
        custom_type: &str,
        renderer: MessageRenderer,
    ) -> anyhow::Result<()>;

    /// Register a transformer for user and assistant Markdown before Pi renders it.
    fn register_markdown_transformer(&self, transformer: MarkdownTransformer)
    -> anyhow::Result<()>;

    /// Register a custom renderer for CustomEntry.
    fn register_entry_renderer(
        &self,
        custom_type: &str,
        renderer: EntryRenderer,
    ) -> anyhow::Result<()>;

    /// Choose how tool calls are drawn. Resolvers run in extension load order.
    fn register_tool_renderer(&self, resolver: ToolRendererResolver) -> anyhow::Result<()>;

    // ----- Actions -----

    /// Send a custom message to the session.
    fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()>;

    /// Send a user message to the agent. Always triggers a turn.
    fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()>;

    /// Append a custom entry to the session for state persistence (not sent to LLM).
    fn append_entry(&self, custom_type: &str, data: Option<Value>) -> anyhow::Result<()>;

    // ----- Session Metadata -----

    /// Set the session display name (shown in session selector).
    fn set_session_name(&self, name: &str) -> anyhow::Result<()>;

    /// Get the current session name, if set.
    fn get_session_name(&self) -> anyhow::Result<Option<String>>;

    /// Set or clear a label on an entry.
    fn set_label(&self, entry_id: &str, label: Option<&str>) -> anyhow::Result<()>;

    /// Execute a shell command.
    async fn exec(
        &self,
        command: &str,
        args: &[String],
        options: ExecOptions,
    ) -> anyhow::Result<ExecResult>;

    /// Get the names of the active tools, which are the tools declared to the model.
    fn get_active_tools(&self) -> anyhow::Result<Vec<String>>;

    /// Get all configured tools with parameter schema, prompt guidelines, exposure, and source metadata.
    fn get_all_tools(&self) -> anyhow::Result<Vec<ToolInfo>>;

    /// Get a copy of the effective settings.
    fn get_settings(&self) -> anyhow::Result<Settings>;

    /// Set the active tools by name. Unknown and `hidden` tools are ignored.
    fn set_active_tools(&self, tool_names: Vec<String>) -> anyhow::Result<()>;

    /// Get available slash commands in the current session.
    fn get_commands(&self) -> anyhow::Result<Vec<SlashCommandInfo>>;

    // ----- Model and Thinking Level -----

    /// Set the model for the current session. Returns false if auth is not configured.
    async fn set_model(&self, model: Model) -> anyhow::Result<bool>;

    /// Get current thinking level.
    fn get_thinking_level(&self) -> anyhow::Result<ThinkingLevel>;

    /// Set the thinking level (clamped to model capabilities) for the current session.
    fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()>;

    // ----- Provider Registration -----

    /// Register or override a model provider. Queued during initial load, immediate afterwards.
    fn register_provider(&self, registration: ProviderRegistration) -> anyhow::Result<()>;

    /// Unregister a previously registered provider.
    fn unregister_provider(&self, name: &str) -> anyhow::Result<()>;

    // ----- MCP Servers -----

    /// Register an MCP server for this session (same config as an `mcpServers` entry).
    fn register_mcp_server(&self, name: &str, config: McpServerConfig) -> anyhow::Result<()>;

    /// Remove an MCP server this extension registered and close its connection.
    fn unregister_mcp_server(&self, name: &str) -> anyhow::Result<()>;

    /// Every MCP server registered by extensions.
    fn get_mcp_servers(&self) -> anyhow::Result<Vec<RegisteredMcpServer>>;

    // ----- Virtual Models -----

    /// Register a virtual model that routes each request to a physical model.
    fn register_virtual_model(&self, model: ExtensionVirtualModel) -> anyhow::Result<()>;

    /// Remove a virtual model registered with `register_virtual_model()`.
    fn unregister_virtual_model(&self, provider: &str, id: &str) -> anyhow::Result<()>;

    /// Shared event bus for extension communication.
    fn events(&self) -> Arc<dyn EventBus>;

    /// Path of the extension this API belongs to (used in error messages of bridges).
    fn extension_path(&self) -> Option<String> {
        None
    }
}

/// Shared handle to the `pi` object.
pub type ExtensionAPIRef = Arc<dyn ExtensionAPI>;

// ============================================================================
// Provider Registration Types
// ============================================================================

/// `route(request, ctx)` of a virtual model registered by an extension.
pub type ExtensionVirtualModelRouteFn = Arc<
    dyn Fn(ModelRouteRequest, ExtensionContext) -> BoxFuture<'static, anyhow::Result<ModelRoute>>
        + Send
        + Sync,
>;

/// Virtual model registered via `pi.register_virtual_model()`.
#[derive(Clone)]
pub struct ExtensionVirtualModel {
    /// Provider the virtual model is listed under. May be a provider with physical models.
    pub provider: String,
    /// Model id. Must not be the id of a physical model of `provider`.
    pub id: String,
    pub name: String,
    /// Thinking levels offered for selection. Defaults to `["off"]`.
    pub thinking_levels: Option<Vec<ThinkingLevel>>,
    /// Limits shown before the first response.
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    /// Input types accepted for selection. Defaults to text and images.
    pub input: Option<Vec<ModelInputModality>>,
    /// Like `VirtualModelDefinition.route`, with an extension context.
    pub route: ExtensionVirtualModelRouteFn,
}

/// `streamSimple(model, context, options)` of a custom API.
pub type StreamSimpleFn = Arc<
    dyn Fn(
            Model,
            TranscriptContext,
            Option<pi_ai::types::SimpleStreamOptions>,
        ) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

/// `refreshModels(context)`.
pub type RefreshModelsFn = Arc<
    dyn Fn(RefreshModelsContext) -> BoxFuture<'static, anyhow::Result<Vec<ProviderModelConfig>>>
        + Send
        + Sync,
>;

/// Legacy OAuth provider for /login support (`ProviderConfig.oauth`).
#[derive(Clone)]
pub struct ProviderOAuthConfig {
    /// Display name for the provider in login UI.
    pub name: String,
    /// Whether access through this auth method is backed by a provider subscription.
    pub is_subscription: Option<bool>,
    /// Deprecated; retained for source compatibility.
    pub uses_callback_server: Option<bool>,
    /// Run the login flow, return credentials to persist.
    pub login: Arc<
        dyn Fn(
                Arc<dyn OAuthLoginCallbacks>,
            )
                -> BoxFuture<'static, anyhow::Result<pi_ai::auth::types::OAuthCredentials>>
            + Send
            + Sync,
    >,
    /// Refresh expired credentials, return updated credentials to persist.
    pub refresh_token: Arc<
        dyn Fn(
                pi_ai::auth::types::OAuthCredentials,
                CancellationToken,
            )
                -> BoxFuture<'static, anyhow::Result<pi_ai::auth::types::OAuthCredentials>>
            + Send
            + Sync,
    >,
    /// Convert credentials to API key string for the provider.
    pub get_api_key: Arc<dyn Fn(&pi_ai::auth::types::OAuthCredentials) -> String + Send + Sync>,
    /// Legacy synchronous credential-dependent model projection.
    pub modify_models: Option<
        Arc<dyn Fn(Vec<Model>, &pi_ai::auth::types::OAuthCredentials) -> Vec<Model> + Send + Sync>,
    >,
}

/// Configuration for registering a provider via `pi.register_provider()`.
#[derive(Clone, Default)]
pub struct ProviderConfig {
    /// Display name for the provider in UI.
    pub name: Option<String>,
    /// Base URL for the API endpoint. Required when defining models.
    pub base_url: Option<String>,
    /// API key literal, env interpolation ($ENV_VAR or ${ENV_VAR}), or leading !command.
    pub api_key: Option<String>,
    /// API type. Required at provider or model level when defining models.
    pub api: Option<Api>,
    /// Optional streamSimple handler for custom APIs.
    pub stream_simple: Option<StreamSimpleFn>,
    /// Image-generation implementations keyed by image API.
    pub images: Option<IndexMap<String, Arc<dyn ProviderImages>>>,
    /// Classifier implementations keyed by classifier API.
    pub classifiers: Option<IndexMap<String, Arc<dyn ProviderClassifier>>>,
    /// Custom headers to include in requests.
    pub headers: Option<IndexMap<String, String>>,
    /// If true, adds Authorization: Bearer header with the resolved API key.
    pub auth_header: Option<bool>,
    /// Models to register. If provided, replaces all existing models for this provider.
    pub models: Option<Vec<ProviderModelConfig>>,
    /// Refresh this provider's model list.
    pub refresh_models: Option<RefreshModelsFn>,
    /// OAuth provider for /login support.
    pub oauth: Option<ProviderOAuthConfig>,
}

/// Chat model configuration. Omitted `type` is normalized to `"chat"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChatModelConfig {
    /// Model ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// API type override for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<Api>,
    /// API endpoint URL override for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Supported input types.
    pub input: Vec<ModelInputModality>,
    /// Provider input limits and cache-safe image preprocessing metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    /// Per-million-token cost rates.
    pub cost: ModelCost,
    /// Custom headers for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Whether the model supports extended thinking.
    pub reasoning: bool,
    /// Maps pi thinking levels to provider/model-specific values; null marks a level unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<pi_ai::types::ThinkingLevelMap>,
    /// Best-effort prompt cache lifetime in seconds per retention tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    /// Maximum context window size in tokens.
    pub context_window: u64,
    /// Maximum output tokens.
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<JsonObject>,
    /// OpenAI compatibility settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ModelCompat>,
}

/// Image-generation model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderImageModelConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub input: Vec<ModelInputModality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    pub output: Vec<ModelInputModality>,
}

/// Structured classifier model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderClassifierModelConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub input: Vec<ModelInputModality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    pub context_window: u64,
}

/// Configuration for a model within a provider, discriminated by `type` (absent = `"chat"`).
#[derive(Debug, Clone)]
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub enum ProviderModelConfig {
    Chat(ProviderChatModelConfig),
    Image(ProviderImageModelConfig),
    Classifier(ProviderClassifierModelConfig),
}

impl Serialize for ProviderModelConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let (mut value, tag) = match self {
            Self::Chat(m) => (serde_json::to_value(m).map_err(S::Error::custom)?, None),
            Self::Image(m) => (
                serde_json::to_value(m).map_err(S::Error::custom)?,
                Some("image"),
            ),
            Self::Classifier(m) => (
                serde_json::to_value(m).map_err(S::Error::custom)?,
                Some("classifier"),
            ),
        };
        if let (Some(tag), Value::Object(map)) = (tag, &mut value) {
            map.insert("type".into(), Value::String(tag.into()));
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProviderModelConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut value = Value::deserialize(deserializer)?;
        let tag = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("chat")
            .to_string();
        if let Value::Object(map) = &mut value {
            map.remove("type");
        }
        match tag.as_str() {
            "chat" => serde_json::from_value(value)
                .map(Self::Chat)
                .map_err(D::Error::custom),
            "image" => serde_json::from_value(value)
                .map(Self::Image)
                .map_err(D::Error::custom),
            "classifier" => serde_json::from_value(value)
                .map(Self::Classifier)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!("unknown model type {other:?}"))),
        }
    }
}

/// Extension factory function. Supports both sync and async initialization.
pub type ExtensionFactory =
    Arc<dyn Fn(ExtensionAPIRef) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Named inline extension (`{ name, factory, hidden?, replaceable?, builtin? }`).
#[derive(Clone)]
pub struct NamedInlineExtension {
    /// Display name shown as `<inline:name>` in the startup Extensions list and errors. With
    /// `builtin`, the extension is named `builtin:name`.
    pub name: String,
    pub factory: ExtensionFactory,
    /// Omit this extension from the startup Extensions list.
    pub hidden: Option<bool>,
    /// Leave this extension out when another extension registers a tool, command, or flag with a
    /// name it registers during loading, instead of reporting a conflict.
    pub replaceable: Option<bool>,
    /// Supply the code of the `builtin:<name>` extension instead of loading as an inline extension.
    pub builtin: Option<bool>,
}

/// An inline extension: a bare factory or a named one.
#[derive(Clone)]
pub enum InlineExtension {
    Factory(ExtensionFactory),
    Named(NamedInlineExtension),
}

// ============================================================================
// Loaded Extension Types
// ============================================================================

#[derive(Clone)]
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub source_info: SourceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionFlag {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub flag_type: FlagType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<FlagValue>,
    pub extension_path: String,
}

#[derive(Clone)]
pub struct ExtensionShortcut {
    pub shortcut: KeyId,
    pub description: Option<String>,
    pub handler: ShortcutHandler,
    pub extension_path: String,
}

pub type SendMessageHandler = Arc<dyn Fn(CustomMessageInput, SendMessageOptions) + Send + Sync>;
pub type SendUserMessageHandler = Arc<dyn Fn(UserContent, SendUserMessageOptions) + Send + Sync>;
pub type AppendEntryHandler = Arc<dyn Fn(String, Option<Value>) + Send + Sync>;
pub type SetSessionNameHandler = Arc<dyn Fn(String) + Send + Sync>;
pub type GetSessionNameHandler = Arc<dyn Fn() -> Option<String> + Send + Sync>;
pub type GetActiveToolsHandler = Arc<dyn Fn() -> Vec<String> + Send + Sync>;
pub type GetAllToolsHandler = Arc<dyn Fn() -> Vec<ToolInfo> + Send + Sync>;
pub type GetSettingsHandler = Arc<dyn Fn() -> Settings + Send + Sync>;
pub type GetCommandsHandler = Arc<dyn Fn() -> Vec<SlashCommandInfo> + Send + Sync>;
pub type SetActiveToolsHandler = Arc<dyn Fn(Vec<String>) + Send + Sync>;
pub type RefreshToolsHandler = Arc<dyn Fn() + Send + Sync>;
pub type SetModelHandler =
    Arc<dyn Fn(Model) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>;
pub type GetThinkingLevelHandler = Arc<dyn Fn() -> ThinkingLevel + Send + Sync>;
pub type SetThinkingLevelHandler = Arc<dyn Fn(ThinkingLevel) + Send + Sync>;
pub type SetLabelHandler = Arc<dyn Fn(String, Option<String>) + Send + Sync>;

/// Tool info with name, description, parameter schema, prompt guidelines, and source metadata.
#[derive(Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_guidelines: Option<Vec<String>>,
    pub reaches_tools: Option<ReachesToolsFn>,
    pub exposure: ToolExposure,
    pub namespace: Option<ToolNamespace>,
    pub annotations: Option<ToolAnnotations>,
    pub source_info: SourceInfo,
}

/// A legacy provider-config registration queued during extension loading.
#[derive(Clone)]
pub struct PendingProviderRegistration {
    pub name: String,
    pub config: ProviderConfig,
    pub extension_path: String,
}

/// A native pi-ai provider registration queued during extension loading.
#[derive(Clone)]
pub struct PendingNativeProviderRegistration {
    pub provider: Arc<dyn Provider>,
    pub extension_path: String,
}

/// A virtual model registration queued during extension loading.
#[derive(Clone)]
pub struct PendingVirtualModelRegistration {
    pub definition: VirtualModelDefinition,
    pub extension_path: String,
}

/// Registration backend of an [`ExtensionRuntime`]. The loader installs a queueing implementation;
/// `runner.bindCore()` replaces it with one that calls the model registry directly.
pub trait ExtensionRuntimeHooks: Send + Sync {
    /// Create an extension context. Fails before the runner binds.
    fn create_context(&self) -> anyhow::Result<ExtensionContext>;
    fn register_provider(
        &self,
        name: &str,
        config: ProviderConfig,
        extension_path: Option<&str>,
    ) -> anyhow::Result<()>;
    fn register_native_provider(
        &self,
        provider: Arc<dyn Provider>,
        extension_path: Option<&str>,
    ) -> anyhow::Result<()>;
    fn unregister_provider(&self, name: &str, extension_path: Option<&str>) -> anyhow::Result<()>;
    fn register_virtual_model(
        &self,
        definition: VirtualModelDefinition,
        extension_path: Option<&str>,
    ) -> anyhow::Result<()>;
    fn unregister_virtual_model(&self, provider: &str, id: &str) -> anyhow::Result<()>;
}

/// Shared state created by the loader, used during registration and runtime
/// (TS `ExtensionRuntime = ExtensionRuntimeState & ExtensionActions`).
///
/// Shared as `Arc<ExtensionRuntime>`; fields use interior mutability. `actions` holds throwing
/// stubs until `runner.initialize()` installs the session's implementation.
pub struct ExtensionRuntime {
    /// Flag values (defaults set during registration, CLI values set after).
    pub flag_values: Mutex<IndexMap<String, FlagValue>>,
    /// Legacy provider-config registrations queued during extension loading.
    pub pending_provider_registrations: Mutex<Vec<PendingProviderRegistration>>,
    /// Native pi-ai provider registrations queued during extension loading.
    pub pending_native_provider_registrations: Mutex<Vec<PendingNativeProviderRegistration>>,
    /// Virtual model registrations queued during extension loading.
    pub pending_virtual_model_registrations: Mutex<Vec<PendingVirtualModelRegistration>>,
    /// Servers registered with `pi.register_mcp_server()`.
    pub mcp_servers: Arc<McpServerRegistry>,
    /// `pi.*` action implementations (`ExtensionActions`).
    pub actions: RwLock<Arc<dyn ExtensionActions>>,
    /// Registration backend (`createContext`, `registerProvider`, ...).
    pub hooks: RwLock<Arc<dyn ExtensionRuntimeHooks>>,
    /// `Some(message)` once invalidated.
    pub stale_message: Mutex<Option<String>>,
    /// Event-bus subscriptions released on invalidation.
    pub event_bus_subscriptions: Mutex<Vec<Unsubscribe>>,
}

/// TS name of the state half of [`ExtensionRuntime`].
pub type ExtensionRuntimeState = ExtensionRuntime;

impl ExtensionRuntime {
    /// Errors when this extension instance is stale after runtime replacement.
    pub fn assert_active(&self) -> anyhow::Result<()> {
        match &*self.stale_message.lock() {
            Some(message) => Err(anyhow::anyhow!("{message}")),
            None => Ok(()),
        }
    }

    /// Marks this extension instance as stale after runtime replacement or reload.
    pub fn invalidate(&self, message: Option<&str>) {
        {
            let mut stale = self.stale_message.lock();
            if stale.is_some() {
                return;
            }
            *stale = Some(
                message
                    .unwrap_or(crate::core::extensions::runner::STALE_EXTENSION_CONTEXT_MESSAGE)
                    .to_string(),
            );
        }
        let unsubscribers = std::mem::take(&mut *self.event_bus_subscriptions.lock());
        for unsubscribe in unsubscribers {
            unsubscribe();
        }
    }

    /// Retain an event-bus subscription until this runtime is invalidated.
    pub fn track_event_bus_subscription(self: &Arc<Self>, unsubscribe: Unsubscribe) -> Unsubscribe {
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let tracked: Unsubscribe = {
            let active = active.clone();
            Arc::new(move || {
                if active.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    unsubscribe();
                }
            })
        };
        self.event_bus_subscriptions.lock().push(tracked.clone());
        let weak = Arc::downgrade(self);
        Arc::new(move || {
            if let Some(runtime) = weak.upgrade() {
                runtime
                    .event_bus_subscriptions
                    .lock()
                    .retain(|entry| !Arc::ptr_eq(entry, &tracked));
            }
            tracked();
        })
    }

    /// Create an extension context. Fails before the runner binds.
    pub fn create_context(&self) -> anyhow::Result<ExtensionContext> {
        self.hooks.read().clone().create_context()
    }

    /// Current `pi.*` action implementations.
    pub fn actions(&self) -> Arc<dyn ExtensionActions> {
        self.actions.read().clone()
    }
}

/// Action implementations for `pi.*` API methods.
/// Provided to `runner.initialize()`, copied into the shared runtime.
#[async_trait::async_trait]
pub trait ExtensionActions: Send + Sync {
    fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()>;
    fn send_user_message(
        &self,
        content: UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()>;
    fn append_entry(&self, custom_type: &str, data: Option<Value>) -> anyhow::Result<()>;
    fn set_session_name(&self, name: &str) -> anyhow::Result<()>;
    fn get_session_name(&self) -> anyhow::Result<Option<String>>;
    fn set_label(&self, entry_id: &str, label: Option<&str>) -> anyhow::Result<()>;
    fn get_active_tools(&self) -> anyhow::Result<Vec<String>>;
    fn get_all_tools(&self) -> anyhow::Result<Vec<ToolInfo>>;
    fn get_settings(&self) -> anyhow::Result<Settings>;
    fn set_active_tools(&self, tool_names: Vec<String>) -> anyhow::Result<()>;
    fn refresh_tools(&self) -> anyhow::Result<()>;
    fn get_commands(&self) -> anyhow::Result<Vec<SlashCommandInfo>>;
    async fn set_model(&self, model: Model) -> anyhow::Result<bool>;
    fn get_thinking_level(&self) -> anyhow::Result<ThinkingLevel>;
    fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()>;
}

/// `executeTool(callerId, name, args, options)` backing [`ExtensionToolContext::execute_tool`].
pub type ExecuteToolFn = Arc<
    dyn Fn(String, String, Value, ExecuteToolOptions) -> BoxFuture<'static, AgentToolCallOutcome>
        + Send
        + Sync,
>;

/// Actions for [`ExtensionContext`] (ctx.* in event handlers). Required by all modes.
#[async_trait::async_trait]
pub trait ExtensionContextActions: Send + Sync {
    fn get_model(&self) -> Option<Model>;
    fn get_scoped_models(&self) -> Vec<ScopedModel>;
    fn is_idle(&self) -> bool;
    fn is_project_trusted(&self) -> bool;
    fn get_signal(&self) -> Option<CancellationToken>;
    fn abort(&self);
    fn has_pending_messages(&self) -> bool;
    fn shutdown(&self);
    fn get_context_usage(&self) -> Option<ContextUsage>;
    fn compact(&self, options: CompactOptions);
    fn get_system_prompt(&self) -> String;
    /// Optional in TS.
    fn get_system_prompt_options(&self) -> Option<BuildSystemPromptOptions> {
        None
    }
    /// Backs `ExtensionToolContext.executeTool()`. `None`: nested calls fail.
    fn execute_tool_fn(&self) -> Option<ExecuteToolFn> {
        None
    }
    /// Backs `ExtensionToolContext.tools`.
    fn get_callable_tools(&self) -> Option<Vec<AgentToolRef>> {
        None
    }
}

/// Actions for [`ExtensionCommandContext`] (ctx.* in command handlers).
#[async_trait::async_trait]
pub trait ExtensionCommandContextActions: Send + Sync {
    async fn wait_for_idle(&self) -> anyhow::Result<()>;
    async fn new_session(
        &self,
        options: NewSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome>;
    async fn fork(
        &self,
        entry_id: &str,
        options: ForkOptions,
    ) -> anyhow::Result<SessionReplaceOutcome>;
    async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> anyhow::Result<SessionReplaceOutcome>;
    async fn switch_session(
        &self,
        session_path: &str,
        options: SwitchSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome>;
    async fn reload(&self) -> anyhow::Result<()>;
}

/// Loaded extension with all registered items.
pub struct Extension {
    pub path: String,
    pub resolved_path: String,
    pub hidden: Option<bool>,
    /// See [`NamedInlineExtension::replaceable`].
    pub replaceable: Option<bool>,
    pub source_info: SourceInfo,
    pub handlers: IndexMap<String, Vec<HandlerFn>>,
    pub tools: IndexMap<String, RegisteredTool>,
    pub message_renderers: IndexMap<String, MessageRenderer>,
    pub tool_renderers: Vec<ToolRendererResolver>,
    pub markdown_transformer: Option<MarkdownTransformer>,
    pub entry_renderers: IndexMap<String, EntryRenderer>,
    pub commands: IndexMap<String, RegisteredCommand>,
    pub flags: IndexMap<String, ExtensionFlag>,
    pub shortcuts: IndexMap<KeyId, ExtensionShortcut>,
}

/// Shared, mutable handle to a loaded extension (registration mutates it after load too).
pub type ExtensionRef = Arc<RwLock<Extension>>;

/// `{ path, error }` entry of [`LoadExtensionsResult::errors`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionLoadError {
    pub path: String,
    pub error: String,
}

/// `{ path, warning }` entry of [`LoadExtensionsResult::warnings`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionLoadWarning {
    pub path: String,
    pub warning: String,
}

/// Result of loading extensions.
#[derive(Clone)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<ExtensionRef>,
    pub errors: Vec<ExtensionLoadError>,
    pub warnings: Option<Vec<ExtensionLoadWarning>>,
    /// Shared runtime: actions are throwing stubs until `runner.initialize()`.
    pub runtime: Arc<ExtensionRuntime>,
}

// ============================================================================
// Extension Error
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extension_event_round_trips_with_type_tag() {
        let event = ExtensionEvent::SessionStart(SessionStartEvent {
            reason: SessionStartReason::Resume,
            previous_session_file: Some("/tmp/a.jsonl".into()),
        });
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            json!({"type": "session_start", "reason": "resume", "previousSessionFile": "/tmp/a.jsonl"})
        );
        let back: ExtensionEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back.type_name(), "session_start");
    }

    #[test]
    fn tool_call_event_shape() {
        let value = json!({"type": "tool_call", "toolCallId": "c1", "toolName": "bash", "input": {"command": "ls"}});
        let event: ExtensionEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), value);
    }

    #[test]
    fn input_event_result_tagged_by_action() {
        let r: InputEventResult =
            serde_json::from_value(json!({"action": "transform", "text": "hi"})).unwrap();
        assert!(matches!(r, InputEventResult::Transform { ref text, .. } if text == "hi"));
        assert_eq!(
            serde_json::to_value(InputEventResult::Handled).unwrap(),
            json!({"action": "handled"})
        );
    }

    #[test]
    fn json_results_convert_to_typed() {
        let r = ExtensionEventResult::Json(json!({"block": true, "reason": "no"}));
        let typed = ToolCallEventResult::from_result(r).unwrap().unwrap();
        assert_eq!(typed.block, Some(true));
        assert!(
            ToolCallEventResult::from_result(ExtensionEventResult::Json(Value::Null))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn provider_model_config_defaults_to_chat() {
        let value = json!({
            "id": "m", "name": "M", "input": ["text"], "reasoning": false,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000, "maxTokens": 100
        });
        let config: ProviderModelConfig = serde_json::from_value(value).unwrap();
        assert!(matches!(config, ProviderModelConfig::Chat(_)));
    }
}
