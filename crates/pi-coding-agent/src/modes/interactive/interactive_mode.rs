//! Port of `packages/coding-agent/src/modes/interactive/interactive-mode.ts`.
//!
//! Interactive mode for the coding agent: TUI rendering and user interaction, delegating business
//! logic to `AgentSession`.
//!
//! Concurrency model: [`InteractiveMode`] is a cheap handle over `Arc<Inner>`. Terminal input
//! arrives on the terminal reader thread, session events on agent tasks, and async work (prompts,
//! commands, dialogs) runs on the tokio runtime captured at construction. All mutable state lives
//! in `Inner::state` (a `parking_lot::Mutex`); the lock is never held while calling components,
//! the session, or callbacks.

mod commands;
mod events;
mod extension_ui;
mod resources;
mod selectors;
mod test_hooks;

pub use test_hooks::InteractiveModeTestHooks;

use crate::config::{APP_NAME, APP_TITLE, VERSION, get_agent_dir};
use crate::core::agent_session::{AgentSession, PromptOptions, PromptOptionsStreamingBehavior};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::agent_session_services::AgentSessionRuntimeDiagnostic;
use crate::core::extensions::types::{
    AutocompleteProviderFactory, EditorFactory, WorkingIndicatorOptions,
};
use crate::core::footer_data_provider::FooterDataProvider;
use crate::core::keybindings::KeybindingsManager;
use crate::core::session_manager::SessionEntry;
use crate::core::session_manager::SessionManager;
use crate::core::settings_manager::SettingsManager;
use crate::core::settings_schema::{FullscreenExitOutput, TuiMode};
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::custom_editor::{CustomEditor, CustomEditorOptions};
use crate::modes::interactive::components::footer::FooterComponent;
use crate::modes::interactive::components::status_indicator::{
    IdleStatus, StatusIndicator, StatusIndicatorKind, WorkingStatusIndicator,
};
use crate::modes::interactive::components::themed_text::ThemedText;
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;
use crate::modes::interactive::program_status_reporter::{BlockedStatus, ProgramStatusReporter};
use crate::modes::interactive::theme::theme::{
    ThemeColor, get_editor_theme, set_registered_themes, theme,
};
use crate::modes::interactive::theme::theme_controller::{
    InteractiveThemeController, InteractiveThemeControllerOptions,
};
use crate::utils::version_check::LatestPiRelease;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_ai::types::AssistantMessage;
use pi_ai::types::ImageContent;
use pi_ai::types::SystemMessage;
use pi_ai::types::ToolResultMessage;
use pi_ai::types::Usage;
use pi_ai::types::UserMessage;
use pi_tui::StyleFn;
use pi_tui::autocomplete::AutocompleteProvider;
use pi_tui::components::editor::EditorComponent;
use pi_tui::components::scroll_view::ScrollView;
use pi_tui::components::spacer::Spacer;
use pi_tui::terminal::Terminal;
use pi_tui::tui::{Component, ComponentRef, Container, MouseEvent, MouseEventType, MouseResult, Tui};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc;

pub use crate::modes::interactive::tui_renderer::create_interactive_tui;
pub use crate::modes::interactive::tui_renderer::create_interactive_tui_reference;
use crate::modes::interactive::tui_renderer::InteractiveTuiOptions;

/// Interface for components that can be expanded/collapsed
pub trait Expandable: Send + Sync {
    fn set_expanded(&self, expanded: bool);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveEditorGetCursorResult {
    pub line: i64,
    pub col: i64,
}

/// An editor that reports its cursor (TS: optional `getCursor()`).
pub trait ActiveEditor: EditorComponent + Send + Sync {
    fn get_cursor(&self) -> Option<ActiveEditorGetCursorResult> {
        None
    }
}

/// An editor that can render the working status in its border.
pub trait WorkingStatusEditor: EditorComponent + Send + Sync {
    fn embed_working_status(&self) -> bool;
    fn set_working_status_indicator(&self, indicator: Option<Arc<StatusIndicator>>);
}

impl WorkingStatusEditor for CustomEditor {
    fn embed_working_status(&self) -> bool {
        CustomEditor::embed_working_status(self)
    }
    fn set_working_status_indicator(&self, indicator: Option<Arc<StatusIndicator>>) {
        CustomEditor::set_working_status_indicator(self, indicator)
    }
}

/// TS: `extends ThemedText implements Expandable`
pub struct ExpandableText {
    text: ThemedText,
    expanded: Arc<AtomicBool>,
}

impl ExpandableText {
    pub fn new(
        get_collapsed_text: Arc<dyn Fn() -> String + Send + Sync>,
        get_expanded_text: Arc<dyn Fn() -> String + Send + Sync>,
        expanded: Option<bool>,
        padding_x: Option<i64>,
        padding_y: Option<i64>,
    ) -> Self {
        let state = Arc::new(AtomicBool::new(expanded.unwrap_or(false)));
        let flag = state.clone();
        let build: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
            if flag.load(Ordering::SeqCst) { get_expanded_text() } else { get_collapsed_text() }
        });
        Self { text: ThemedText::new(build, Some(padding_x.unwrap_or(0)), Some(padding_y.unwrap_or(0))), expanded: state }
    }
}

impl Expandable for ExpandableText {
    fn set_expanded(&self, expanded: bool) {
        self.expanded.store(expanded, Ordering::SeqCst);
        self.text.invalidate();
    }
}

impl Component for ExpandableText {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn invalidate(&self) {
        self.text.invalidate();
    }
}

type LogoClickFn = Arc<dyn Fn(i64, i64) + Send + Sync>;

/// The built-in header. Clicking its logo (the first two lines, after one column of padding) plays an easter egg.
/// TS: `extends ExpandableText`
pub struct BuiltInHeader {
    text: ExpandableText,
    on_logo_click: Mutex<Option<LogoClickFn>>,
}

impl BuiltInHeader {
    pub fn new(
        get_collapsed_text: Arc<dyn Fn() -> String + Send + Sync>,
        get_expanded_text: Arc<dyn Fn() -> String + Send + Sync>,
        expanded: Option<bool>,
        padding_x: Option<i64>,
        padding_y: Option<i64>,
    ) -> Self {
        Self {
            text: ExpandableText::new(get_collapsed_text, get_expanded_text, expanded, padding_x, padding_y),
            on_logo_click: Mutex::new(None),
        }
    }

    pub fn on_logo_click(&self) -> Option<Arc<dyn Fn(i64, i64) + Send + Sync>> {
        self.on_logo_click.lock().clone()
    }

    pub fn set_on_logo_click(&self, value: Option<Arc<dyn Fn(i64, i64) + Send + Sync>>) {
        *self.on_logo_click.lock() = value;
    }
}

impl Expandable for BuiltInHeader {
    fn set_expanded(&self, expanded: bool) {
        self.text.set_expanded(expanded);
    }
}

impl Component for BuiltInHeader {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn invalidate(&self) {
        self.text.invalidate();
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        let callback = self.on_logo_click()?;
        if event.kind != MouseEventType::Click || event.y > 1 || event.x < 1 || event.x > 4 {
            return None;
        }
        callback(event.screen_x - event.x + 1, event.screen_y - event.y);
        Some(MouseResult::handled())
    }
}

// TS: Component & { dispose?(): void }
#[derive(Clone)]
pub struct ExtensionDialogComponent {
    pub component: ComponentRef,
    pub dispose: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompactionQueuedMessageMode {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

impl CompactionQueuedMessageMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionQueuedMessage {
    pub text: String,
    pub mode: CompactionQueuedMessageMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompactionCostNoticeKind {
    #[serde(rename = "compaction")]
    Compaction,
    #[serde(rename = "branch_summary")]
    BranchSummary,
}

impl CompactionCostNoticeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::BranchSummary => "branch_summary",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCostNotice {
    /// Always "compaction_cost".
    pub r#type: String,
    pub kind: CompactionCostNoticeKind,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RenderSessionItem {
    SystemMessage(SystemMessage),
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ToolResultMessage(ToolResultMessage),
    Value(Value),
    SessionEntry(SessionEntry),
    CompactionCostNotice(CompactionCostNotice),
}

/// EIO/EPIPE/ENOTCONN/ENOTTY: the terminal is gone.
pub(crate) fn is_dead_terminal_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::EIO || code == libc::EPIPE || code == libc::ENOTCONN || code == libc::ENOTTY)
}

pub fn format_crash_extension_hint(extension_matches: Option<&[String]>) -> Option<String> {
    let matches: Vec<&String> = extension_matches.unwrap_or_default().iter().filter(|m| !m.is_empty()).collect();
    if matches.is_empty() {
        return None;
    }
    let quoted: Vec<String> = matches.iter().map(|m| format!("`{m}`")).collect();
    let labels = match quoted.len() {
        1 => quoted[0].clone(),
        2 => quoted.join(" and "),
        n => format!("{}, and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
    };
    let noun = if matches.len() == 1 { "extension" } else { "extensions" };
    let pronoun = if matches.len() == 1 { "it" } else { "them" };
    Some(format!(
        "A stack frame came from loaded {noun} {labels}, which may be involved. Try disabling {pronoun} with `{APP_NAME} config`, or run `{APP_NAME} -ne` to confirm."
    ))
}

pub(crate) const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str = "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

pub(crate) fn is_anthropic_subscription_auth_key(api_key: Option<&str>) -> bool {
    api_key.is_some_and(|k| k.starts_with("sk-ant-oat"))
}

pub(crate) fn is_unknown_model(model: Option<&pi_ai::types::Model>) -> bool {
    model.is_some_and(|m| m.provider == "unknown" && m.id == "unknown" && m.api == "unknown")
}

pub(crate) fn quote_if_needed(value: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./~:@".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn format_resume_command(session_manager: Arc<SessionManager>) -> Option<String> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    format_resume_command_unchecked(&session_manager)
}

/// [`format_resume_command`] without the TTY check.
pub(crate) fn format_resume_command_unchecked(session_manager: &SessionManager) -> Option<String> {
    if !session_manager.is_persisted() {
        return None;
    }
    let session_file = session_manager.get_session_file()?;
    if !std::path::Path::new(&session_file).exists() {
        return None;
    }
    let mut args = vec![APP_NAME.to_string()];
    if !session_manager.uses_default_session_dir() {
        args.push("--session-dir".to_string());
        args.push(quote_if_needed(&session_manager.get_session_dir()));
    }
    args.push("--session".to_string());
    args.push(session_manager.get_session_id());
    Some(args.join(" "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoginProviderCompletionOptionAuthTypes {
    #[serde(rename = "oauth")]
    Oauth,
    #[serde(rename = "api_key")]
    ApiKey,
}

impl LoginProviderCompletionOptionAuthTypes {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::ApiKey => "api_key",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProviderCompletionOption {
    pub id: String,
    pub name: String,
    pub auth_types: Vec<LoginProviderCompletionOptionAuthTypes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<bool>,
}

/// The process signal operations behind suspend-to-background (TS `handleCtrlZ` uses `process.on`,
/// `process.once` and `process.kill`). [`ProcessSuspendSignals`] is the real implementation; tests
/// inject a fake through [`InteractiveModeTestHooks::set_suspend_signals`].
pub trait SuspendSignals: Send + Sync {
    /// Ignore SIGINT until the returned function restores the previous disposition.
    fn ignore_sigint(&self) -> Box<dyn FnOnce() + Send>;
    /// Run `callback` once on the next SIGCONT. The listener is armed before this returns; the returned
    /// function removes it.
    fn once_sigcont(&self, callback: Box<dyn FnOnce() + Send>) -> Box<dyn FnOnce() + Send>;
    /// Send SIGTSTP to the process group (`process.kill(0, "SIGTSTP")`).
    fn send_sigtstp(&self) -> std::io::Result<()>;
}

/// [`SuspendSignals`] for the current process.
pub struct ProcessSuspendSignals {
    rt: tokio::runtime::Handle,
}

impl ProcessSuspendSignals {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self { rt }
    }
}

impl SuspendSignals for ProcessSuspendSignals {
    fn ignore_sigint(&self) -> Box<dyn FnOnce() + Send> {
        #[cfg(unix)]
        {
            // SAFETY: replaces and later restores the SIGINT disposition.
            let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
            Box::new(move || unsafe {
                libc::signal(libc::SIGINT, previous);
            })
        }
        #[cfg(not(unix))]
        {
            Box::new(|| {})
        }
    }

    fn once_sigcont(&self, callback: Box<dyn FnOnce() + Send>) -> Box<dyn FnOnce() + Send> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            // Register the listener now: a SIGCONT that arrives before a spawned task registers it is lost.
            let _guard = self.rt.enter();
            let Ok(mut stream) = signal(SignalKind::from_raw(libc::SIGCONT)) else {
                return Box::new(|| {});
            };
            let task = self.rt.spawn(crate::utils::uncaught::supervise(async move {
                if stream.recv().await.is_some() {
                    callback();
                }
            }));
            Box::new(move || task.abort())
        }
        #[cfg(not(unix))]
        {
            let _ = callback;
            Box::new(|| {})
        }
    }

    fn send_sigtstp(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            // SAFETY: plain syscall.
            if unsafe { libc::kill(0, libc::SIGTSTP) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Options for InteractiveMode initialization.
#[derive(Clone, Default)]
pub struct InteractiveModeOptions {
    /// Providers that were migrated to auth.json (shows warning)
    pub migrated_providers: Option<Vec<String>>,
    /// Diagnostics collected before the interactive TUI was initialized.
    pub startup_diagnostics: Option<Vec<AgentSessionRuntimeDiagnostic>>,
    /// Warning message if session model couldn't be restored
    pub model_fallback_message: Option<String>,
    /// Cwd to trust after reload if it gained a .pi directory during this implicitly trusted session.
    pub auto_trust_on_reload_cwd: Option<String>,
    /// Initial message to send on startup (can include @file content)
    pub initial_message: Option<String>,
    /// Images to attach to the initial message
    pub initial_images: Option<Vec<ImageContent>>,
    /// Additional messages to send after the initial message
    pub initial_messages: Option<Vec<String>>,
    /// Force verbose startup (overrides quietStartup setting)
    pub verbose: Option<bool>,
    /// TUI layout mode.
    pub tui_mode: Option<TuiMode>,
    /// Initial interactive theme setting for this invocation.
    pub initial_theme_setting: Option<String>,
    /// Terminal implementation. Defaults to the current process terminal.
    pub terminal: Option<Arc<dyn Terminal>>,
}

pub(crate) type Handler = Arc<dyn Fn() + Send + Sync>;

/// A component shown in place of the editor by an extension (select, confirm, input, editor, custom).
#[derive(Clone)]
pub(crate) struct DialogEntry {
    pub(crate) id: u64,
    pub(crate) component: ComponentRef,
    pub(crate) blocked: Option<BlockedStatus>,
    pub(crate) dispose: Option<Handler>,
}

#[derive(Clone)]
pub(crate) struct TerminalInputSubscription {
    pub(crate) id: u64,
    pub(crate) handler: crate::core::extensions::types::TerminalInputHandler,
    pub(crate) listener_id: u64,
}

pub(crate) struct State {
    pub(crate) is_initialized: bool,
    pub(crate) tui_mode: TuiMode,
    pub(crate) editor: Arc<dyn EditorComponent>,
    pub(crate) editor_is_default: bool,
    pub(crate) editor_border_color: Option<StyleFn>,
    pub(crate) editor_component_factory: Option<EditorFactory>,
    pub(crate) autocomplete_provider: Option<Arc<dyn AutocompleteProvider>>,
    pub(crate) autocomplete_provider_wrappers: Vec<AutocompleteProviderFactory>,
    pub(crate) fd_path: Option<String>,
    pub(crate) active_selector_token: Option<u64>,
    pub(crate) active_selector_dispose: Option<Handler>,
    pub(crate) next_id: u64,
    pub(crate) active_status_indicator: Option<Arc<StatusIndicator>>,
    pub(crate) active_working_indicator_embedded: bool,
    pub(crate) working_message: Option<String>,
    pub(crate) working_visible: bool,
    pub(crate) working_indicator_options: Option<WorkingIndicatorOptions>,
    pub(crate) hidden_thinking_label: String,
    pub(crate) last_sigint_time: i64,
    pub(crate) last_escape_time: i64,
    pub(crate) changelog_markdown: Option<String>,
    pub(crate) startup_notices_shown: bool,
    pub(crate) anthropic_subscription_warning_shown: bool,
    pub(crate) last_status_spacer: Option<ComponentRef>,
    pub(crate) last_status_text: Option<ComponentRef>,
    pub(crate) managed_tool_status_started: bool,
    pub(crate) streaming_component: Option<Arc<AssistantMessageComponent>>,
    pub(crate) streaming_message: Option<AssistantMessage>,
    pub(crate) entries_rendered_by_boundary_compaction: HashSet<String>,
    pub(crate) pending_tools: IndexMap<String, Arc<ToolExecutionComponent>>,
    pub(crate) tool_output_expanded: bool,
    pub(crate) hide_thinking_block: bool,
    pub(crate) output_pad: i64,
    pub(crate) skill_commands: IndexMap<String, String>,
    pub(crate) unsubscribe: Option<Handler>,
    pub(crate) is_bash_mode: bool,
    pub(crate) bash_component: Option<Arc<BashExecutionComponent>>,
    pub(crate) pending_bash_components: Vec<Arc<BashExecutionComponent>>,
    pub(crate) auto_compaction_escape_handler: Option<Option<Handler>>,
    pub(crate) retry_escape_handler: Option<Option<Handler>>,
    pub(crate) compaction_queued_messages: Vec<CompactionQueuedMessage>,
    pub(crate) shutdown_requested: bool,
    pub(crate) is_shutting_down: bool,
    pub(crate) bug_report_hint_shown: bool,
    pub(crate) install_change_warning_shown: bool,
    pub(crate) extension_dialogs: Vec<DialogEntry>,
    pub(crate) extension_terminal_input_subscriptions: Vec<TerminalInputSubscription>,
    pub(crate) extension_widgets_above: IndexMap<String, ComponentRef>,
    pub(crate) extension_widgets_below: IndexMap<String, ComponentRef>,
    pub(crate) custom_footer: Option<ComponentRef>,
    pub(crate) built_in_header: Option<ComponentRef>,
    pub(crate) custom_header: Option<ComponentRef>,
    pub(crate) auto_trust_on_reload_cwd: Option<String>,
    pub(crate) transcript_scroll_view: Option<Arc<ScrollView>>,
    pub(crate) fullscreen_layout_root: Option<ComponentRef>,
    pub(crate) main_screen_render_state: Option<pi_tui::tui_main_screen::TuiMainScreenRenderState>,
    pub(crate) submit_enabled: bool,
    /// Renderers replaced by `switch_tui_mode`; they forward to the active one.
    pub(crate) retired_renderers: Vec<Arc<Tui>>,
    /// Process signal operations used by suspend (Ctrl+Z); tests inject a fake.
    pub(crate) suspend_signals: Option<Arc<dyn SuspendSignals>>,
}

pub(crate) struct Inner {
    pub(crate) weak: Weak<Inner>,
    pub(crate) runtime_host: Arc<AgentSessionRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) options: InteractiveModeOptions,
    pub(crate) renderer: Mutex<Arc<Tui>>,
    pub(crate) header_container: Arc<Container>,
    pub(crate) loaded_resources_container: Arc<Container>,
    pub(crate) chat_container: Arc<Container>,
    pub(crate) document_container: Arc<Container>,
    pub(crate) pending_messages_container: Arc<Container>,
    pub(crate) status_container: Arc<Container>,
    pub(crate) widget_container_above: Arc<Container>,
    pub(crate) widget_container_below: Arc<Container>,
    pub(crate) editor_container: Arc<Container>,
    pub(crate) footer_container: Arc<Container>,
    pub(crate) default_editor: Arc<CustomEditor>,
    pub(crate) footer: Arc<FooterComponent>,
    pub(crate) footer_data_provider: Arc<FooterDataProvider>,
    pub(crate) keybindings: Arc<KeybindingsManager>,
    pub(crate) version: String,
    pub(crate) idle_status: Arc<IdleStatus>,
    pub(crate) last_status_message: Arc<Mutex<String>>,
    pub(crate) program_status: ProgramStatusReporter,
    pub(crate) theme_controller: Arc<InteractiveThemeController>,
    pub(crate) input_tx: mpsc::UnboundedSender<String>,
    pub(crate) input_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    /// Generation of the registered SIGTERM/SIGHUP handlers; 0 while unregistered.
    pub(crate) signal_generation: std::sync::atomic::AtomicU64,
    pub(crate) state: Mutex<State>,
}

pub(crate) const DEFAULT_WORKING_MESSAGE: &str = "Working";
pub(crate) const DEFAULT_HIDDEN_THINKING_LABEL: &str = "Thinking...";
/// Maximum total widget lines to prevent viewport overflow.
pub(crate) const MAX_WIDGET_LINES: usize = 10;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn err_message(error: &anyhow::Error) -> String {
    error.to_string()
}

#[derive(Clone)]
pub struct InteractiveMode {
    inner: Arc<Inner>,
}

impl InteractiveMode {
    pub fn new(runtime_host: Arc<AgentSessionRuntime>, options: Option<&InteractiveModeOptions>) -> Self {
        Self { inner: Inner::new(runtime_host, options.cloned().unwrap_or_default()) }
    }

    pub async fn init(&self) -> anyhow::Result<()> {
        self.inner.clone().init().await
    }

    /// Run the interactive mode. This is the main entry point.
    /// Initializes the UI, shows warnings, processes initial messages, and starts the interactive loop.
    pub async fn run(&self) -> anyhow::Result<()> {
        use futures::FutureExt;
        // A panic escaping the main loop is uncaught (TS: the `uncaughtException` handler).
        match std::panic::AssertUnwindSafe(self.inner.clone().run()).catch_unwind().await {
            Ok(result) => result,
            Err(payload) => crate::utils::uncaught::report_uncaught(payload),
        }
    }

    pub fn render_initial_messages(&self) {
        self.inner.render_initial_messages();
    }

    pub async fn get_user_input(&self) -> anyhow::Result<String> {
        self.inner.get_user_input().await
    }

    pub fn clear_editor(&self) {
        self.inner.clear_editor();
    }

    pub fn show_error(&self, error_message: &str) {
        self.inner.show_error(error_message);
    }

    pub fn show_warning(&self, warning_message: &str) {
        self.inner.show_warning(warning_message);
    }

    /// Show a dim status line; immediately-sequential status messages replace each other.
    pub fn show_status(&self, message: &str) {
        self.inner.show_status(message);
    }

    pub fn show_new_version_notification(&self, release: &LatestPiRelease) {
        self.inner.show_new_version_notification(release);
    }

    pub fn show_package_update_notification(&self, packages: &[String]) {
        self.inner.show_package_update_notification(packages);
    }

    pub fn stop(&self, fullscreen_exit_output: Option<FullscreenExitOutput>) {
        self.inner.stop(fullscreen_exit_output);
    }

    /// The active renderer (test and embedding hook).
    pub fn tui(&self) -> Arc<Tui> {
        self.inner.ui()
    }

    /// Submit text as if typed into the editor and confirmed (test and embedding hook).
    pub fn submit(&self, text: &str) {
        self.inner.clone().handle_submit(text.to_string());
    }
}

impl Inner {
    fn new(runtime_host: Arc<AgentSessionRuntime>, mut options: InteractiveModeOptions) -> Arc<Inner> {
        let session = runtime_host.session();
        let settings = session.settings_manager();
        pi_tui::terminal_image::set_capability_overrides(settings.get_terminal_capability_overrides());
        let tui_mode = options.tui_mode.unwrap_or_else(|| settings.get_tui_mode());
        options.tui_mode = Some(tui_mode);
        let rt = tokio::runtime::Handle::current();
        install_future_spawner(&rt);

        let (input_tx, input_rx) = mpsc::unbounded_channel();

        Arc::new_cyclic(|weak: &Weak<Inner>| {
            let on_right_click_paste: Handler = {
                let weak = weak.clone();
                Arc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        let paste = this.clone();
                        this.spawn(async move { paste.handle_right_click_paste().await });
                    }
                })
            };
            let renderer = create_interactive_tui(&InteractiveTuiOptions {
                tui_mode,
                show_hardware_cursor: settings.get_show_hardware_cursor(),
                log_directory: get_agent_dir(),
                terminal: options.terminal.clone(),
                on_right_click_paste: Some(on_right_click_paste),
                fullscreen_copy_on_select: Some(settings.get_fullscreen_copy_on_select()),
                fullscreen_wheel_scroll_lines: Some(settings.get_fullscreen_wheel_scroll_lines()),
            });
            renderer.set_clear_on_shrink(settings.get_clear_on_shrink());

            let header_container = Container::new_ref();
            let loaded_resources_container = Container::new_ref();
            let chat_container = Container::new_ref();
            let document_container = Container::new_ref();
            document_container.add_child(header_container.clone());
            document_container.add_child(loaded_resources_container.clone());
            document_container.add_child(chat_container.clone());

            let keybindings = KeybindingsManager::create(None);
            pi_tui::keybindings::set_keybindings(keybindings.tui_manager());
            let default_editor = Arc::new(CustomEditor::new(
                renderer.clone(),
                get_editor_theme(),
                keybindings.clone(),
                Some(&CustomEditorOptions {
                    padding_x: Some(settings.get_editor_padding_x()),
                    autocomplete_max_visible: Some(settings.get_autocomplete_max_visible()),
                    embed_working_status: Some(true),
                }),
            ));
            let editor_container = Container::new_ref();
            editor_container.add_child(default_editor.clone());
            let footer_data_provider = Arc::new(FooterDataProvider::new(&session.session_manager().get_cwd()));
            let footer = Arc::new(FooterComponent::new(session.clone(), &footer_data_provider));
            footer.set_auto_compact_enabled(session.auto_compaction_enabled());
            let footer_container = Container::new_ref();
            footer_container.add_child(footer.clone());

            set_registered_themes(session.resource_loader().get_themes().themes);
            let theme_controller = {
                let weak_settings = weak.clone();
                let weak_error = weak.clone();
                let weak_changed = weak.clone();
                InteractiveThemeController::new(
                    renderer.clone(),
                    &InteractiveThemeControllerOptions {
                        get_settings_manager: {
                            let host = runtime_host.clone();
                            let _ = weak_settings;
                            Arc::new(move || host.session().settings_manager())
                        },
                        show_error: Arc::new(move |message: String| {
                            if let Some(this) = weak_error.upgrade() {
                                this.show_error(&message);
                            }
                        }),
                        on_changed: Arc::new(move || {
                            if let Some(this) = weak_changed.upgrade() {
                                this.update_editor_border_color();
                            }
                        }),
                        initial_theme_setting: options.initial_theme_setting.clone(),
                    },
                )
            };

            let program_status = {
                let weak_terminal = weak.clone();
                let host = runtime_host.clone();
                let fallback_terminal = renderer.terminal().clone();
                ProgramStatusReporter::new(
                    Arc::new(move || {
                        weak_terminal.upgrade().map(|this| this.ui().terminal().clone()).unwrap_or_else(|| fallback_terminal.clone())
                    }),
                    Arc::new(move || host.session().session_manager().get_session_name()),
                )
            };

            let default_editor_dyn: Arc<dyn EditorComponent> = default_editor.clone();
            let state = State {
                is_initialized: false,
                tui_mode,
                editor: default_editor_dyn,
                editor_is_default: true,
                editor_border_color: None,
                editor_component_factory: None,
                autocomplete_provider: None,
                autocomplete_provider_wrappers: Vec::new(),
                fd_path: None,
                active_selector_token: None,
                active_selector_dispose: None,
                next_id: 1,
                active_status_indicator: None,
                active_working_indicator_embedded: false,
                working_message: None,
                working_visible: true,
                working_indicator_options: None,
                hidden_thinking_label: DEFAULT_HIDDEN_THINKING_LABEL.to_string(),
                last_sigint_time: 0,
                last_escape_time: 0,
                changelog_markdown: None,
                startup_notices_shown: false,
                anthropic_subscription_warning_shown: false,
                last_status_spacer: None,
                last_status_text: None,
                managed_tool_status_started: false,
                streaming_component: None,
                streaming_message: None,
                entries_rendered_by_boundary_compaction: HashSet::new(),
                pending_tools: IndexMap::new(),
                tool_output_expanded: false,
                hide_thinking_block: settings.get_hide_thinking_block(),
                output_pad: settings.get_output_pad(),
                skill_commands: IndexMap::new(),
                unsubscribe: None,
                is_bash_mode: false,
                bash_component: None,
                pending_bash_components: Vec::new(),
                auto_compaction_escape_handler: None,
                retry_escape_handler: None,
                compaction_queued_messages: Vec::new(),
                shutdown_requested: false,
                is_shutting_down: false,
                bug_report_hint_shown: false,
                install_change_warning_shown: false,
                extension_dialogs: Vec::new(),
                extension_terminal_input_subscriptions: Vec::new(),
                extension_widgets_above: IndexMap::new(),
                extension_widgets_below: IndexMap::new(),
                custom_footer: None,
                built_in_header: None,
                custom_header: None,
                auto_trust_on_reload_cwd: options.auto_trust_on_reload_cwd.clone(),
                transcript_scroll_view: None,
                fullscreen_layout_root: None,
                main_screen_render_state: None,
                submit_enabled: false,
                retired_renderers: Vec::new(),
                suspend_signals: None,
            };

            Inner {
                weak: weak.clone(),
                runtime_host: runtime_host.clone(),
                rt,
                options,
                renderer: Mutex::new(renderer),
                header_container,
                loaded_resources_container,
                chat_container,
                document_container,
                pending_messages_container: Container::new_ref(),
                status_container: Container::new_ref(),
                widget_container_above: Container::new_ref(),
                widget_container_below: Container::new_ref(),
                editor_container,
                footer_container,
                default_editor,
                footer,
                footer_data_provider,
                keybindings,
                version: VERSION.to_string(),
                idle_status: Arc::new(IdleStatus::new()),
                last_status_message: Arc::new(Mutex::new(String::new())),
                program_status,
                theme_controller,
                input_tx,
                input_rx: tokio::sync::Mutex::new(input_rx),
                signal_generation: std::sync::atomic::AtomicU64::new(0),
                state: Mutex::new(state),
            }
        })
        .after_new()
    }

    fn after_new(self: Arc<Self>) -> Arc<Self> {
        let weak = self.weak.clone();
        self.runtime_host.set_before_session_invalidate(Some(Arc::new(move || {
            if let Some(this) = weak.upgrade() {
                this.reset_extension_ui();
            }
        })));
        let weak = self.weak.clone();
        self.runtime_host.set_rebind_session(Some(Arc::new(move |_session: Arc<AgentSession>| {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(this) = weak.upgrade() {
                    this.clone().rebind_current_session(true).await?;
                    this.theme_controller.apply_from_settings();
                }
                Ok(())
            })
        })));
        self
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub(crate) fn this(&self) -> Arc<Inner> {
        self.weak.upgrade().expect("interactive mode is alive")
    }

    pub(crate) fn session(&self) -> Arc<AgentSession> {
        self.runtime_host.session()
    }

    pub(crate) fn session_manager(&self) -> Arc<SessionManager> {
        self.session().session_manager()
    }

    pub(crate) fn settings_manager(&self) -> Arc<SettingsManager> {
        self.session().settings_manager()
    }

    pub(crate) fn ui(&self) -> Arc<Tui> {
        self.renderer.lock().clone()
    }

    pub(crate) fn editor(&self) -> Arc<dyn EditorComponent> {
        self.state.lock().editor.clone()
    }

    pub(crate) fn editor_ref(&self) -> ComponentRef {
        
        (self.editor()) as _
    }

    pub(crate) fn output_pad(&self) -> i64 {
        self.state.lock().output_pad
    }

    pub(crate) fn request_render(&self) {
        self.ui().request_render(false);
    }

    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Detached like a promise nobody awaits: a panic is uncaught (TS `unhandledRejection`).
        // Each segment between awaits updates the UI atomically, like JS (see `ui_lock::atomic`).
        self.rt.spawn(crate::utils::uncaught::supervise(pi_tui::ui_lock::atomic(future)));
    }

    /// Run `future` up to its first suspension point now, like calling an async function in JS, and
    /// finish it on the runtime. Keeps the synchronous part of event handlers in event order.
    pub(crate) fn start_now<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let _guard = self.rt.enter();
        // Each segment between awaits updates the UI atomically, like JS (see `ui_lock::atomic`).
        let mut future = Box::pin(crate::utils::uncaught::supervise(pi_tui::ui_lock::atomic(future)));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        if future.as_mut().poll(&mut cx).is_pending() {
            self.rt.spawn(future);
        }
    }

    pub(crate) fn next_id(&self) -> u64 {
        let mut s = self.state.lock();
        let id = s.next_id;
        s.next_id += 1;
        id
    }

    pub(crate) fn tui_mode(&self) -> TuiMode {
        self.state.lock().tui_mode
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    async fn init(self: Arc<Self>) -> anyhow::Result<()> {
        if self.state.lock().is_initialized {
            return Ok(());
        }

        self.register_signal_handlers();

        // Load changelog (only show new entries, skip for resumed sessions)
        let changelog = self.get_changelog_for_display();
        self.state.lock().changelog_markdown = changelog;

        let session = self.session();
        let scoped = session.scoped_models();
        if !scoped.is_empty() && self.should_show_startup_details() {
            let model_list = scoped
                .iter()
                .map(|sm| {
                    let thinking = sm.thinking_level.map(|l| format!(":{}", l.as_str())).unwrap_or_default();
                    format!("{}{thinking}", sm.model.id)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let cycle_keys = self.keybindings.get_keys("app.model.cycleForward");
            let cycle_hint = if cycle_keys.is_empty() {
                String::new()
            } else {
                theme().fg(
                    &ThemeColor::Muted,
                    &format!(
                        " ({} to cycle)",
                        crate::modes::interactive::components::keybinding_hints::format_key_text(
                            &cycle_keys.join("/"),
                            Some(&crate::modes::interactive::components::keybinding_hints::KeyTextFormatOptions {
                                capitalize: Some(true)
                            })
                        )
                    ),
                )
            };
            println!("{}", theme().fg(&ThemeColor::Dim, &format!("Model scope: {model_list}{cycle_hint}")));
        }

        // Keep one component tree and remount it when changing renderers.
        self.render_widgets();
        let settings = self.settings_manager();
        let viewport = crate::modes::interactive::chat_viewport::create_chat_viewport(
            &crate::modes::interactive::chat_viewport::ChatViewportOptions {
                document: self.document_container.clone(),
                pending_messages: self.pending_messages_container.clone(),
                status: self.status_container.clone(),
                widgets_above: Some(self.widget_container_above.clone()),
                editor: self.editor_container.clone(),
                widgets_below: Some(self.widget_container_below.clone()),
                footer: self.footer_container.clone(),
                scrollbar: Some(settings.get_fullscreen_scrollbar()),
                scrollbar_track_style: Some(Arc::new(|text: String| theme().fg(&ThemeColor::ScrollbarTrack, &text))),
                scrollbar_thumb_style: Some(Arc::new(|text: String| theme().fg(&ThemeColor::ScrollbarThumb, &text))),
            },
        );
        {
            let mut s = self.state.lock();
            s.transcript_scroll_view = Some(viewport.transcript.clone());
            s.fullscreen_layout_root = Some(viewport.root.clone());
        }
        let ui = self.ui();
        self.mount_interactive_tui(&ui, &self.root_components());

        // Accept text while startup completes, but only enable interrupt, exit, and submission feedback.
        {
            let weak = self.weak.clone();
            self.default_editor.on_action(
                &"app.clear".to_string(),
                Arc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.handle_ctrl_c();
                    }
                }),
            );
            let weak = self.weak.clone();
            self.default_editor.set_on_ctrl_d(Some(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.handle_ctrl_d();
                }
            })));
            self.install_submit_callback();
        }
        ui.set_focus(Some(self.editor_ref()));

        // Start the UI before initializing extensions so session_start handlers can use interactive dialogs
        ui.start();
        self.state.lock().is_initialized = true;
        self.program_status.report();
        self.ensure_png_transcoder();

        self.theme_controller.apply_from_settings();
        // The header and startup notices bake theme colors into their text, so build them once the terminal
        // reported its colors.
        let _ = self.theme_controller.wait_for_terminal_colors().await;

        self.build_header();
        self.request_render();

        // Ensure fd and rg are available after mounting the TUI.
        let status_cb: Arc<dyn Fn(crate::utils::tools_manager::ToolStatus) + Send + Sync> = {
            let weak = self.weak.clone();
            Arc::new(move |status| {
                if let Some(this) = weak.upgrade() {
                    this.show_managed_tool_status(&status);
                }
            })
        };
        let (fd_path, _rg) = tokio::join!(
            crate::utils::tools_manager::ensure_tool(
                crate::utils::tools_manager::GetToolPathTool::Fd,
                Some(status_cb.clone())
            ),
            crate::utils::tools_manager::ensure_tool(crate::utils::tools_manager::GetToolPathTool::Rg, Some(status_cb)),
        );
        self.state.lock().fd_path = fd_path.ok().flatten();

        // Enable the remaining input handlers only after managed-tool setup completes.
        self.setup_key_handlers();
        self.state.lock().submit_enabled = true;
        self.request_render();

        // Initialize extensions first so resources are shown before messages
        self.clone().rebind_current_session(false).await?;

        // Render initial messages AFTER showing loaded resources
        self.render_initial_messages();

        // Set up theme file watcher
        {
            let weak = self.weak.clone();
            crate::modes::interactive::theme::theme::on_theme_change(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    let ui = this.ui();
                    ui.invalidate();
                    this.update_editor_border_color();
                    ui.request_render(false);
                }
            }));
        }

        // Set up git branch watcher
        {
            let weak = self.weak.clone();
            let _ = self.footer_data_provider.on_branch_change(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.request_render();
                }
            }));
        }

        self.update_available_provider_count();

        // Flush the completed startup state before loading the remaining syntax grammars.
        self.ui().render_now(false);
        let weak = self.weak.clone();
        self.spawn(async move {
            let _ = crate::utils::syntax_highlight::load_all_highlight_languages().await;
            if let Some(this) = weak.upgrade() {
                if !this.state.lock().is_initialized {
                    return;
                }
                this.ui().invalidate();
                this.request_render();
            }
            // Startup is done: return the memory its transient allocations left behind.
            crate::utils::memory::release_free_memory_in_background();
        });
        Ok(())
    }

    /// The components mounted on the renderer, in order.
    fn root_components(&self) -> Vec<ComponentRef> {
        vec![
            self.document_container.clone(),
            self.pending_messages_container.clone(),
            self.status_container.clone(),
            self.widget_container_above.clone(),
            self.editor_container.clone(),
            self.widget_container_below.clone(),
            self.footer_container.clone(),
        ]
    }

    fn build_header(&self) {
        use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text, raw_key_hint};
        use crate::modes::interactive::components::pi_logo::{pi_logo_lines, pi_wordmark, supports_pi_logo};

        if self.should_show_startup_header() {
            let show_details = self.should_show_startup_details();
            let show_logo = supports_pi_logo();
            let version = self.version.clone();
            let with_logo = move |hints: String| -> String {
                if !show_logo {
                    return format!("{} {}\n{hints}", pi_wordmark(), theme().fg(&ThemeColor::Dim, &format!("v{version}")));
                }
                let (top, bottom) = pi_logo_lines();
                format!("{top} {}\n{bottom} {hints}", theme().fg(&ThemeColor::Dim, &format!("v{version}")))
            };
            let hint = |kb: &str, desc: &str| key_hint(kb.to_string(), desc);
            let expanded_instructions = move || -> String {
                [
                    hint("app.interrupt", "to interrupt"),
                    hint("app.clear", "to clear"),
                    raw_key_hint(&format!("{} twice", key_text("app.clear".to_string())), "to exit"),
                    hint("app.exit", "to exit (empty)"),
                    hint("app.suspend", "to suspend"),
                    hint("tui.editor.deleteToLineEnd", "to delete to end"),
                    hint("app.thinking.cycle", "to cycle thinking level"),
                    raw_key_hint(
                        &format!(
                            "{}/{}",
                            key_text("app.model.cycleForward".to_string()),
                            key_text("app.model.cycleBackward".to_string())
                        ),
                        "to cycle models",
                    ),
                    hint("app.model.select", "to select model"),
                    hint("app.tools.expand", "to expand tools"),
                    hint("app.thinking.toggle", "to expand thinking"),
                    hint("app.editor.external", "for external editor"),
                    raw_key_hint("/", "for commands"),
                    raw_key_hint("!", "to run bash"),
                    raw_key_hint("!!", "to run bash (no context)"),
                    hint("app.message.followUp", "to queue follow-up"),
                    hint("app.message.dequeue", "to edit all queued messages"),
                    hint("app.clipboard.pasteImage", "to paste files on macOS, images, or text"),
                    raw_key_hint("drop files", "to attach"),
                ]
                .join("\n")
            };
            let compact_instructions = move || -> String {
                [
                    hint("app.interrupt", "interrupt"),
                    raw_key_hint(
                        &format!("{}/{}", key_text("app.clear".to_string()), key_text("app.exit".to_string())),
                        "clear/exit",
                    ),
                    raw_key_hint("/", "commands"),
                    raw_key_hint("!", "bash"),
                    hint("app.tools.expand", "more"),
                ]
                .join(&theme().fg(&ThemeColor::Muted, " · "))
            };
            let compact_onboarding = move || -> String {
                theme().fg(
                    &ThemeColor::Dim,
                    &format!(
                        "Press {} to show full startup help{}.",
                        key_text("app.tools.expand".to_string()),
                        if show_details { " and loaded resources" } else { "" }
                    ),
                )
            };
            let onboarding = || -> String {
                theme().fg(
                    &ThemeColor::Dim,
                    "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.",
                )
            };
            let with_logo = Arc::new(with_logo);
            let wl1 = with_logo.clone();
            let wl2 = with_logo;
            let header = Arc::new(BuiltInHeader::new(
                Arc::new(move || {
                    format!("{}\n{}\n\n{}", wl1(compact_instructions()), compact_onboarding(), onboarding())
                }),
                Arc::new(move || format!("{}\n\n{}", wl2(expanded_instructions()), onboarding())),
                Some(self.get_startup_expansion_state()),
                Some(1),
                Some(0),
            ));
            if show_logo {
                let weak = self.weak.clone();
                header.set_on_logo_click(Some(Arc::new(move |column, row| {
                    if let Some(this) = weak.upgrade() {
                        commands::play_easter_egg_3d(&this, crate::modes::interactive::components::easter_egg_3d::EasterEgg3d::PiLogo { column, row });
                    }
                })));
            }
            let header_ref: ComponentRef = header;
            self.state.lock().built_in_header = Some(header_ref.clone());
            self.header_container.add_child(Arc::new(Spacer::new(1)));
            self.header_container.add_child(header_ref);
            self.header_container.add_child(Arc::new(Spacer::new(1)));
        } else {
            // Minimal header when silenced
            let header: ComponentRef = Arc::new(pi_tui::components::text::Text::new("", 0, 0, None));
            self.state.lock().built_in_header = Some(header.clone());
            self.header_container.add_child(header);
        }
    }

    /// Update terminal title with session name and cwd.
    pub(crate) fn update_terminal_title(&self) {
        let sm = self.session_manager();
        let cwd = sm.get_cwd();
        let cwd_basename = std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(cwd.clone());
        let title = match sm.get_session_name() {
            Some(name) => format!("{APP_TITLE} - {name} - {cwd_basename}"),
            None => format!("{APP_TITLE} - {cwd_basename}"),
        };
        self.ui().terminal().set_title(&title);
    }

    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        self.clone().init().await?;

        if std::env::var_os("PI_OFFLINE").is_none_or(|v| v.is_empty()) {
            let this = self.clone();
            self.spawn(async move {
                let signal = tokio_util::sync::CancellationToken::new();
                let timeout_signal = signal.clone();
                let timer = crate::utils::uncaught::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    timeout_signal.cancel();
                });
                let runtime = this.session().model_runtime();
                if crate::modes::interactive::model_catalog_refresh::refresh_model_catalogs(&runtime, signal)
                    .await
                    .is_ok()
                {
                    this.update_available_provider_count();
                }
                timer.abort();
            });
        }

        // Start version check asynchronously
        {
            let this = self.clone();
            let version = self.version.clone();
            self.spawn(async move {
                if let Ok(Some(release)) = crate::utils::version_check::check_for_new_pi_version(&version).await {
                    this.show_new_version_notification(&release);
                }
            });
        }

        // Start package update check asynchronously
        {
            let this = self.clone();
            self.spawn(async move {
                let updates = this.check_for_package_updates().await;
                if !updates.is_empty() {
                    this.show_package_update_notification(&updates);
                }
                // On Windows, npm can overwrite the shared console title while checking extension
                // package versions. Restore Pi's title after the startup check.
                if cfg!(windows) && this.state.lock().is_initialized {
                    this.update_terminal_title();
                }
            });
        }

        // Check tmux keyboard setup asynchronously
        {
            let this = self.clone();
            self.spawn(async move {
                if let Some(warning) = check_tmux_keyboard_setup().await {
                    this.show_warning(&warning);
                }
            });
        }

        // Show startup warnings
        for diagnostic in self.options.startup_diagnostics.clone().unwrap_or_default() {
            match diagnostic.r#type.as_str() {
                "error" => self.show_error(&diagnostic.message),
                "warning" => self.show_warning(&diagnostic.message),
                _ => self.show_status(&diagnostic.message),
            }
        }
        if let Some(migrated) = &self.options.migrated_providers
            && !migrated.is_empty()
        {
            self.show_warning(&format!("Migrated credentials to auth.json: {}", migrated.join(", ")));
        }
        if let Some(error) = self.session().model_runtime().get_error() {
            self.show_error(&format!("models.json error: {error}"));
        }
        if let Some(message) = &self.options.model_fallback_message {
            self.show_warning(message);
        }
        if let Some(crash) = crate::core::crash_log::take_unnotified_crash(None, None) {
            let when = chrono::DateTime::parse_from_rfc3339(&crash.timestamp)
                .ok()
                .map(|t| t.with_timezone(&chrono::Local).format("%-m/%-d/%Y, %-I:%M:%S %p").to_string())
                .unwrap_or_default();
            self.show_warning(&format!(
                "{APP_NAME} crashed on {when} ({}). Run /bug to report it; the crash details are attached automatically.",
                crash.message
            ));
        }

        {
            let this = self.clone();
            self.spawn(async move { this.maybe_warn_about_anthropic_subscription_auth(None).await });
        }

        // Process initial messages
        if let Some(initial) = self.options.initial_message.clone() {
            let options = PromptOptions { images: self.options.initial_images.clone(), ..Default::default() };
            if let Err(error) = self.session().prompt(&initial, Some(&options)).await {
                self.show_error(&err_message(&error));
            }
        }
        for message in self.options.initial_messages.clone().unwrap_or_default() {
            if let Err(error) = self.session().prompt(&message, None).await {
                self.show_error(&err_message(&error));
            }
        }

        // Main interactive loop
        loop {
            let user_input = self.get_user_input().await?;
            if let Err(error) = self.session().prompt(&user_input, None).await {
                self.show_error(&err_message(&error));
            }
        }
    }

    async fn check_for_package_updates(&self) -> Vec<String> {
        if std::env::var_os("PI_OFFLINE").is_some_and(|v| !v.is_empty()) {
            return Vec::new();
        }
        let package_manager = crate::core::package_manager::DefaultPackageManager::new(
            &crate::core::package_manager::PackageManagerOptions {
                cwd: self.session_manager().get_cwd(),
                agent_dir: get_agent_dir(),
                settings_manager: self.settings_manager(),
                builtin_extensions: None,
            },
        );
        match package_manager.check_for_available_updates().await {
            Ok(updates) => updates.into_iter().map(|u| u.display_name).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Get changelog entries to display on startup.
    fn get_changelog_for_display(&self) -> Option<String> {
        use crate::utils::changelog::{get_changelog_path, get_new_entries, normalize_changelog_links, parse_changelog};
        // Skip changelog for resumed/continued sessions (already have messages)
        if !self.session().state().messages.is_empty() {
            return None;
        }
        let settings = self.settings_manager();
        let last_version = settings.get_last_changelog_version();
        let entries = parse_changelog(&get_changelog_path());
        let Some(last_version) = last_version else {
            // Fresh install - record the version, send telemetry, don't show changelog
            settings.set_last_changelog_version(VERSION);
            self.report_install_telemetry(VERSION);
            return None;
        };
        let new_entries = get_new_entries(&entries, &last_version);
        if !new_entries.is_empty() {
            settings.set_last_changelog_version(VERSION);
            self.report_install_telemetry(VERSION);
            return Some(
                new_entries.iter().map(|e| normalize_changelog_links(&e.content, &e.into())).collect::<Vec<_>>().join("\n\n"),
            );
        }
        None
    }

    fn report_install_telemetry(&self, version: &str) {
        if std::env::var_os("PI_OFFLINE").is_some_and(|v| !v.is_empty()) {
            return;
        }
        if !crate::core::telemetry::is_install_telemetry_enabled(self.settings_manager(), None) {
            return;
        }
        let url = format!("https://pi.dev/api/report-install?version={}", url_encode(version));
        let user_agent = crate::utils::pi_user_agent::get_pi_user_agent(version);
        self.spawn(async move {
            let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build();
            if let Ok(client) = client {
                let _ = client.get(url).header("User-Agent", user_agent).send().await;
            }
        });
    }

    pub(crate) fn get_markdown_theme_with_settings(&self) -> pi_tui::components::markdown::MarkdownTheme {
        let mut md = crate::modes::interactive::theme::theme::get_markdown_theme();
        md.code_block_indent = Some(self.settings_manager().get_code_block_indent());
        md
    }

    pub(crate) fn get_startup_expansion_state(&self) -> bool {
        self.options.verbose.unwrap_or(false) || self.state.lock().tool_output_expanded
    }

    /// Startup header (logo, version, key hints). Hidden only by quietStartup: true.
    pub(crate) fn should_show_startup_header(&self) -> bool {
        use crate::core::settings_schema::QuietStartupValue;
        self.options.verbose == Some(true)
            || !matches!(self.settings_manager().get_quiet_startup(), QuietStartupValue::Bool(true))
    }

    /// Startup details (model scope, loaded resources). Hidden by quietStartup: true or "header".
    pub(crate) fn should_show_startup_details(&self) -> bool {
        use crate::core::settings_schema::QuietStartupValue;
        self.options.verbose == Some(true)
            || matches!(self.settings_manager().get_quiet_startup(), QuietStartupValue::Bool(false))
    }

    pub(crate) fn mount_interactive_tui(&self, tui: &Arc<Tui>, components: &[ComponentRef]) {
        for component in components {
            tui.add_child(component.clone());
        }
        if tui.is_viewport_tui() {
            let root = self.state.lock().fullscreen_layout_root.clone();
            tui.set_layout_root(root);
        }
    }

    pub(crate) fn stop_interactive_tui(&self, fullscreen_exit_output: FullscreenExitOutput) {
        let renderer = self.ui();
        if renderer.mode() == pi_tui::tui::TuiMode::Fullscreen && fullscreen_exit_output == FullscreenExitOutput::Transcript {
            while renderer.has_overlay() {
                renderer.hide_overlay();
            }
            self.switch_tui_mode(TuiMode::Regular, false, false);
            self.ui().render_now(false);
        }
        let ui = self.ui();
        let preserve_screen = ui.mode() == pi_tui::tui::TuiMode::Fullscreen;
        ui.stop(pi_tui::tui::TuiStopOptions { preserve_screen });
    }

    /// Replace the renderer (regular/fullscreen), moving the component tree over. Returns false while
    /// overlays are open.
    pub(crate) fn switch_tui_mode(&self, mode: TuiMode, restore_progress: bool, start_renderer: bool) -> bool {
        let previous_ui = self.ui();
        let previous_mode = if previous_ui.mode() == pi_tui::tui::TuiMode::Fullscreen {
            TuiMode::Fullscreen
        } else {
            TuiMode::Regular
        };
        if mode == previous_mode {
            return true;
        }
        if previous_ui.has_overlay() {
            return false;
        }
        let components = previous_ui.children();
        let focus = previous_ui.get_focused_component();
        let terminal = previous_ui.terminal().clone();
        let show_hardware_cursor = previous_ui.get_show_hardware_cursor();
        let clear_on_shrink = previous_ui.get_clear_on_shrink();
        if !previous_ui.is_viewport_tui() {
            self.state.lock().main_screen_render_state = Some(previous_ui.capture_render_state());
        }
        previous_ui.stop(pi_tui::tui::TuiStopOptions { preserve_screen: true });
        previous_ui.set_focus(None);
        previous_ui.clear();
        if previous_ui.is_viewport_tui() {
            previous_ui.set_layout_root(None);
        }

        let settings = self.settings_manager();
        let weak = self.weak.clone();
        let next_ui = create_interactive_tui(&InteractiveTuiOptions {
            tui_mode: mode,
            show_hardware_cursor,
            log_directory: get_agent_dir(),
            terminal: Some(terminal.clone()),
            on_right_click_paste: Some(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    let paste = this.clone();
                        this.spawn(async move { paste.handle_right_click_paste().await });
                }
            })),
            fullscreen_copy_on_select: Some(settings.get_fullscreen_copy_on_select()),
            fullscreen_wheel_scroll_lines: Some(settings.get_fullscreen_wheel_scroll_lines()),
        });
        next_ui.set_clear_on_shrink(clear_on_shrink);
        let main_screen_render_state = self.state.lock().main_screen_render_state.clone();
        if !next_ui.is_viewport_tui()
            && let Some(state) = main_screen_render_state
        {
            next_ui.restore_render_state(&state);
        }
        *self.renderer.lock() = next_ui.clone();
        // Components keep the renderer they were built with (TS hands them a proxy that follows the
        // active renderer), so retired renderers forward to the new one and stay alive for them.
        {
            let mut s = self.state.lock();
            s.retired_renderers.push(previous_ui.clone());
            for retired in &s.retired_renderers {
                retired.redirect_to(&next_ui);
            }
            s.tui_mode = mode;
        }
        self.mount_interactive_tui(&next_ui, &components);
        next_ui.invalidate();
        next_ui.set_focus(focus);
        if !start_renderer {
            return true;
        }
        next_ui.start();
        self.theme_controller.set_tui(next_ui.clone());
        self.theme_controller.rebind_tui();
        self.rebind_extension_terminal_input_listeners();
        let weak = self.weak.clone();
        next_ui.set_on_debug(Some(Arc::new(move || {
            if let Some(this) = weak.upgrade() {
                this.handle_debug_command();
            }
        })));
        let session = self.session();
        if restore_progress && settings.get_show_terminal_progress() && (session.is_streaming() || session.is_compacting()) {
            terminal.set_progress(true);
        }
        true
    }

    // ------------------------------------------------------------------
    // Runtime settings & session rebinding
    // ------------------------------------------------------------------

    pub(crate) fn apply_fullscreen_scrollbar_setting(&self) {
        let view = self.state.lock().transcript_scroll_view.clone();
        if let Some(view) = view {
            view.set_scrollbar(self.settings_manager().get_fullscreen_scrollbar());
        }
    }

    /// Lets extension images use the PNG transcoder; tool results register it themselves.
    pub(crate) fn ensure_png_transcoder(&self) {
        let weak = self.weak.clone();
        crate::utils::image_convert::ensure_png_transcoder(Arc::new(move || {
            if let Some(this) = weak.upgrade() {
                this.ui().invalidate();
                this.request_render();
            }
        }));
    }

    pub(crate) fn apply_runtime_settings(&self) {
        let settings = self.settings_manager();
        pi_tui::terminal_image::set_capability_overrides(settings.get_terminal_capability_overrides());
        self.ensure_png_transcoder();
        let _ = crate::core::http_dispatcher::configure_http_dispatcher(settings.get_http_idle_timeout_ms().ok());
        self.apply_fullscreen_scrollbar_setting();
        let ui = self.ui();
        if ui.is_viewport_tui() {
            ui.set_copy_on_select(settings.get_fullscreen_copy_on_select());
            ui.set_wheel_scroll_lines(crate::modes::interactive::tui_renderer::to_tui_wheel_lines(
                &Some(settings.get_fullscreen_wheel_scroll_lines()),
            ));
        }
        let session = self.session();
        self.footer.set_session(session.clone());
        self.footer.set_auto_compact_enabled(session.auto_compaction_enabled());
        self.footer_data_provider.set_cwd(&self.session_manager().get_cwd());
        let has_indicator = {
            let mut s = self.state.lock();
            s.hide_thinking_block = settings.get_hide_thinking_block();
            s.output_pad = settings.get_output_pad();
            s.active_status_indicator.is_some()
        };
        ui.set_show_hardware_cursor(settings.get_show_hardware_cursor());
        let clear_on_shrink = settings.get_clear_on_shrink();
        ui.set_clear_on_shrink(clear_on_shrink);
        if !clear_on_shrink && !has_indicator {
            self.status_container.clear();
        }
        let padding = settings.get_editor_padding_x().max(0) as usize;
        let max_visible = settings.get_autocomplete_max_visible().max(0) as usize;
        self.default_editor.set_padding_x(padding);
        self.default_editor.set_autocomplete_max_visible(max_visible);
        let (editor, is_default) = {
            let s = self.state.lock();
            (s.editor.clone(), s.editor_is_default)
        };
        if !is_default {
            editor.set_padding_x(padding);
            editor.set_autocomplete_max_visible(max_visible);
        }
    }

    pub(crate) async fn rebind_current_session(self: Arc<Self>, render_before_bind: bool) -> anyhow::Result<()> {
        let session = self.session();
        let unsubscribe = self.state.lock().unsubscribe.take();
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe();
        }
        self.program_status.reset();
        self.apply_runtime_settings();

        if render_before_bind {
            self.render_current_session_state();
            self.subscribe_to_agent();
        }

        self.clone().bind_current_session_extensions().await?;

        if !Arc::ptr_eq(&self.session(), &session) {
            return Ok(());
        }
        if !render_before_bind {
            self.subscribe_to_agent();
        }
        self.update_available_provider_count();
        self.update_editor_border_color();
        self.update_terminal_title();
        Ok(())
    }

    /// Show a fatal error, record the crash, stop the UI and exit (TS returns `never`).
    pub(crate) fn handle_fatal_runtime_error(&self, prefix: &str, error: &anyhow::Error) -> ! {
        let message = err_message(error);
        self.show_error(&format!("{prefix}: {message}"));
        let pad = self.output_pad();
        if let Some(hint) = self.get_crash_extension_hint(error) {
            self.chat_container
                .add_child(Arc::new(ThemedText::new(Arc::new(move || theme().fg(&ThemeColor::Warning, &hint)), Some(pad), Some(0))));
        }
        if self.record_crash(crate::core::crash_log::CrashRecordKind::FatalError, error) {
            let instructions = self.crash_report_instructions();
            self.chat_container.add_child(Arc::new(ThemedText::new(
                Arc::new(move || theme().fg(&ThemeColor::Muted, &instructions)),
                Some(pad),
                Some(0),
            )));
        }
        crate::modes::interactive::theme::theme::stop_theme_watcher();
        self.stop(Some(FullscreenExitOutput::Transcript));
        crate::utils::process_exit::exit(1);
    }

    fn get_crash_extension_hint(&self, error: &anyhow::Error) -> Option<String> {
        let stack = format!("{error:?}");
        let extensions: Vec<crate::core::crash_log::ExtensionStackMetadata> = self
            .session()
            .resource_loader()
            .get_extensions()
            .extensions
            .iter()
            .map(|extension| {
                let extension = extension.read();
                crate::core::crash_log::ExtensionStackMetadata {
                    path: extension.path.clone(),
                    resolved_path: extension.resolved_path.clone(),
                    source_info: extension.source_info.clone(),
                }
            })
            .collect();
        let matches = crate::core::crash_log::find_extension_stack_matches(Some(&stack), &extensions);
        format_crash_extension_hint(Some(&matches))
    }

    /// Persist a crash so the next start can point the user at `/bug`. Returns false when nothing was written.
    fn record_crash(&self, kind: crate::core::crash_log::CrashRecordKind, error: &anyhow::Error) -> bool {
        let session = self.session();
        let crash = crate::core::crash_log::RecordCrashCrash {
            kind,
            error: crate::core::crash_log::error_to_value(error),
            session_file: session.session_file(),
            cwd: session.session_manager().get_cwd(),
        };
        crate::core::crash_log::record_crash(&crash, None).is_some()
    }

    fn crash_report_instructions(&self) -> String {
        let resume = if self.session().session_file().is_some() {
            format!("run `{APP_NAME} -r` to resume the session, then")
        } else {
            "start pi and".to_string()
        };
        format!("To report this crash: {resume} run /bug. The crash details are attached automatically.")
    }

    fn suggest_bug_report(&self) {
        {
            let mut s = self.state.lock();
            if s.bug_report_hint_shown {
                return;
            }
            s.bug_report_hint_shown = true;
        }
        let pad = self.output_pad();
        self.chat_container.add_child(Arc::new(ThemedText::new(
            Arc::new(|| {
                theme().fg(&ThemeColor::Muted, &format!("If this looks like a {APP_NAME} bug, /bug sends a report to the developers."))
            }),
            Some(pad),
            Some(0),
        )));
        self.request_render();
    }

    pub(crate) fn maybe_suggest_bug_report(&self, message: &AssistantMessage) {
        if message.stop_reason != pi_ai::types::StopReason::Error || pi_ai::utils::retry::is_retryable_assistant_error(message) {
            return;
        }
        static ABORT_PATTERN: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\b(?:abort(?:ed)?|cancel(?:l?ed)?)\b").unwrap());
        if ABORT_PATTERN.is_match(message.error_message.as_deref().unwrap_or("")) {
            return;
        }
        if self.maybe_show_install_change_warning() {
            return;
        }
        self.suggest_bug_report();
    }

    /// After an error, check whether an update replaced or removed this install while the session ran.
    pub(crate) fn maybe_show_install_change_warning(&self) -> bool {
        if self.state.lock().install_change_warning_shown {
            return true;
        }
        let Some(change) = crate::config::detect_install_change(None) else { return false };
        self.state.lock().install_change_warning_shown = true;
        let cause = match &change {
            crate::config::InstallChange::Updated { version } => {
                format!("{APP_NAME} was updated to {version} while this session was running ({VERSION})")
            }
            _ => format!("The {APP_NAME} installation this session runs from was removed or replaced"),
        };
        let restart = match format_resume_command(self.session_manager()) {
            Some(cmd) => format!("Restart with `{cmd}` to continue this session."),
            None => format!("Restart {APP_NAME}."),
        };
        self.show_warning(&format!("{cause}. Features that load code on demand can fail until restart. {restart}"));
        true
    }

    pub(crate) fn render_current_session_state(&self) {
        self.loaded_resources_container.clear();
        self.chat_container.clear();
        self.pending_messages_container.clear();
        {
            let mut s = self.state.lock();
            s.compaction_queued_messages.clear();
            s.streaming_component = None;
            s.streaming_message = None;
            s.pending_tools.clear();
        }
        self.render_initial_messages();
    }

    // ------------------------------------------------------------------
    // Status indicators
    // ------------------------------------------------------------------

    /// The active editor as a working-status editor, when it embeds the status.
    fn embedded_status_editor(&self) -> Option<Arc<CustomEditor>> {
        let editor = self.editor();
        let component: ComponentRef = editor;
        let custom = crate::modes::interactive::components::custom_editor::as_custom_editor(&component)?;
        custom.embed_working_status().then_some(custom)
    }

    pub(crate) fn is_embedded_status(&self) -> bool {
        self.embedded_status_editor().is_some()
    }

    fn set_editor_working_status_indicator(&self, indicator: Option<Arc<StatusIndicator>>) -> bool {
        self.default_editor.set_working_status_indicator(None);
        let Some(editor) = self.embedded_status_editor() else { return false };
        editor.set_working_status_indicator(indicator);
        true
    }

    pub(crate) fn show_status_indicator(&self, indicator: Arc<StatusIndicator>) {
        let previous = self.state.lock().active_status_indicator.replace(indicator.clone());
        if let Some(previous) = previous {
            previous.dispose();
        }
        self.state.lock().active_working_indicator_embedded = false;
        self.status_container.clear();
        self.set_editor_working_status_indicator(None);
        if self.set_editor_working_status_indicator(Some(indicator.clone())) {
            self.state.lock().active_working_indicator_embedded = true;
            return;
        }
        self.status_container.add_child(indicator);
    }

    pub(crate) fn clear_status_indicator(&self, kind: Option<StatusIndicatorKind>) {
        let (cleared, was_embedded) = {
            let mut s = self.state.lock();
            if let Some(kind) = kind
                && s.active_status_indicator.as_ref().map(|i| i.kind()) != Some(kind)
            {
                return;
            }
            let cleared = s.active_status_indicator.take();
            let was_embedded = s.active_working_indicator_embedded;
            s.active_working_indicator_embedded = false;
            (cleared, was_embedded)
        };
        if let Some(indicator) = &cleared {
            indicator.dispose();
        }
        self.status_container.clear();
        self.set_editor_working_status_indicator(None);
        if cleared.is_some() && !was_embedded && self.tui_mode() == TuiMode::Regular && self.ui().get_clear_on_shrink() {
            self.status_container.add_child(self.idle_status.clone());
        }
    }

    /// Color function for indicators embedded in the editor border.
    pub(crate) fn embedded_indicator_color(&self) -> Option<Arc<dyn Fn(String) -> String + Send + Sync>> {
        if !self.is_embedded_status() {
            return None;
        }
        let weak = self.weak.clone();
        Some(Arc::new(move |text: String| {
            let Some(this) = weak.upgrade() else { return text };
            let border = this.state.lock().editor_border_color.clone();
            match border {
                Some(color) => color(&text),
                None => {
                    let level = this.session().thinking_level();
                    (theme().get_thinking_border_color(level))(text)
                }
            }
        }))
    }

    pub(crate) fn show_working_status_indicator(&self) {
        let color_fn = self.embedded_indicator_color();
        let (message, options) = {
            let s = self.state.lock();
            (
                s.working_message.clone().unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_string()),
                s.working_indicator_options.clone(),
            )
        };
        let indicator = WorkingStatusIndicator::new(self.ui(), &message, options.as_ref(), color_fn);
        self.show_status_indicator(indicator.indicator());
    }

    pub(crate) fn active_indicator_kind(&self) -> Option<StatusIndicatorKind> {
        self.state.lock().active_status_indicator.as_ref().map(|i| i.kind())
    }

    pub(crate) fn set_working_visible(&self, visible: bool) {
        self.state.lock().working_visible = visible;
        if !visible {
            self.clear_status_indicator(Some(StatusIndicatorKind::Working));
            self.request_render();
            return;
        }
        if self.session().is_streaming() && self.active_indicator_kind() != Some(StatusIndicatorKind::Working) {
            self.show_working_status_indicator();
        }
        self.request_render();
    }

    pub(crate) fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>) {
        let active = {
            let mut s = self.state.lock();
            s.working_indicator_options = options.clone();
            s.active_status_indicator.clone()
        };
        if let Some(active) = active
            && active.kind() == StatusIndicatorKind::Working
        {
            active.loader().set_indicator(options.map(|o| pi_tui::components::loader::LoaderIndicatorOptions {
                frames: o.frames,
                interval_ms: o.interval_ms,
            }));
        }
        self.request_render();
    }

    pub(crate) fn set_hidden_thinking_label(&self, label: Option<&str>) {
        let label = label.unwrap_or(DEFAULT_HIDDEN_THINKING_LABEL).to_string();
        let streaming = {
            let mut s = self.state.lock();
            s.hidden_thinking_label = label.clone();
            s.streaming_component.clone()
        };
        for child in self.chat_container.children() {
            if let Some(component) = pi_tui::tui::downcast_component::<AssistantMessageComponent>(&child) {
                component.set_hidden_thinking_label(&label);
            }
        }
        if let Some(streaming) = streaming {
            streaming.set_hidden_thinking_label(&label);
        }
        self.request_render();
    }

    // ------------------------------------------------------------------
    // Chat helpers
    // ------------------------------------------------------------------

    pub(crate) fn add_text(&self, container: &Container, build: Arc<dyn Fn() -> String + Send + Sync>, padding_x: i64, padding_y: i64) {
        container.add_child(Arc::new(ThemedText::new(build, Some(padding_x), Some(padding_y))));
    }

    pub(crate) fn add_spacer(&self) {
        self.chat_container.add_child(Arc::new(Spacer::new(1)));
    }

    /// Show a managed-tool status update in the chat.
    fn show_managed_tool_status(&self, status: &crate::utils::tools_manager::ToolStatus) {
        use crate::utils::tools_manager::ToolStatusType;
        let start = {
            let mut s = self.state.lock();
            let start = !s.managed_tool_status_started;
            s.managed_tool_status_started = true;
            start
        };
        if start {
            self.add_spacer();
        }
        let warning = status.r#type == ToolStatusType::Warning;
        let message = if warning { format!("Warning: {}", status.message) } else { status.message.clone() };
        let color = if warning { ThemeColor::Warning } else { ThemeColor::Dim };
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&color, &message)), 1, 0);
        {
            let mut s = self.state.lock();
            s.last_status_spacer = None;
            s.last_status_text = None;
        }
        self.request_render();
    }

    /// Show a status message in the chat. Back-to-back status messages update the previous line.
    pub(crate) fn show_status(&self, message: &str) {
        let children = self.chat_container.children();
        let last = children.last().cloned();
        let second_last = if children.len() > 1 { children.get(children.len() - 2).cloned() } else { None };
        let (status_text, status_spacer) = {
            let s = self.state.lock();
            (s.last_status_text.clone(), s.last_status_spacer.clone())
        };
        if let (Some(last), Some(second_last), Some(text), Some(spacer)) = (&last, &second_last, &status_text, &status_spacer)
            && pi_tui::tui::same_component(last, text)
            && pi_tui::tui::same_component(second_last, spacer)
        {
            *self.last_status_message.lock() = message.to_string();
            text.invalidate();
            self.request_render();
            return;
        }
        let spacer: ComponentRef = Arc::new(Spacer::new(1));
        *self.last_status_message.lock() = message.to_string();
        let shared = self.last_status_message.clone();
        let text: ComponentRef = Arc::new(ThemedText::new(
            Arc::new(move || theme().fg(&ThemeColor::Dim, &shared.lock())),
            Some(1),
            Some(0),
        ));
        self.chat_container.add_child(spacer.clone());
        self.chat_container.add_child(text.clone());
        {
            let mut s = self.state.lock();
            s.last_status_spacer = Some(spacer);
            s.last_status_text = Some(text);
        }
        self.request_render();
    }

    pub(crate) fn render_initial_messages(&self) {
        let sm = self.session_manager();
        let entries = sm.build_context_entries();
        self.render_session_entries(&entries, true, true);
        self.render_project_trust_warning_if_needed();

        // Show compaction info if session was compacted
        let compaction_count = sm.get_entries().iter().filter(|e| matches!(e, SessionEntry::Compaction(_))).count();
        if compaction_count > 0 {
            let times = if compaction_count == 1 { "1 time".to_string() } else { format!("{compaction_count} times") };
            self.show_status(&format!("Session compacted {times}"));
        }
    }

    fn render_project_trust_warning_if_needed(&self) {
        if self.settings_manager().is_project_trusted()
            || !crate::core::trust_manager::has_trust_requiring_project_resources(&self.session_manager().get_cwd())
        {
            return;
        }
        if !self.chat_container.children().is_empty() {
            self.add_spacer();
        }
        self.add_text(
            &self.chat_container,
            Arc::new(|| {
                theme().fg(
                    &ThemeColor::Warning,
                    &format!(
                        "This project is not trusted. Project {} resources and packages are ignored. Use /trust to save a trust decision, then restart pi.",
                        crate::config::CONFIG_DIR_NAME
                    ),
                )
            }),
            1,
            0,
        );
    }

    pub(crate) async fn get_user_input(&self) -> anyhow::Result<String> {
        let mut rx = self.input_rx.lock().await;
        rx.recv().await.ok_or_else(|| anyhow::anyhow!("input closed"))
    }

    pub(crate) fn rebuild_chat_from_messages(&self) {
        self.chat_container.clear();
        let entries = self.session_manager().build_context_entries();
        self.render_session_entries(&entries, false, false);
    }

    // ------------------------------------------------------------------
    // Key handlers
    // ------------------------------------------------------------------

    pub(crate) fn handle_ctrl_c(&self) {
        let now = now_ms();
        let last = self.state.lock().last_sigint_time;
        if now - last < 500 {
            let this = self.this();
            self.spawn(async move { this.shutdown(false).await });
        } else {
            self.clear_editor();
            self.state.lock().last_sigint_time = now;
        }
    }

    pub(crate) fn handle_ctrl_d(&self) {
        // Only called when editor is empty (enforced by CustomEditor)
        let this = self.this();
        self.spawn(async move { this.shutdown(false).await });
    }

    /// Gracefully shut down: stop the TUI, dispose the runtime, print the resume hint and exit.
    pub(crate) async fn shutdown(self: Arc<Self>, from_signal: bool) {
        {
            let mut s = self.state.lock();
            if s.is_shutting_down {
                return;
            }
            s.is_shutting_down = true;
        }
        if from_signal {
            let _ = self.runtime_host.dispose().await;
            self.theme_controller.disable_auto_sync();
            self.ui().terminal().drain_input(std::time::Duration::from_millis(1000), std::time::Duration::from_millis(50));
            self.stop(None);
            crate::utils::process_exit::exit(0);
        }

        self.theme_controller.disable_auto_sync();
        self.ui().terminal().drain_input(std::time::Duration::from_millis(1000), std::time::Duration::from_millis(50));
        self.stop(None);
        let _ = self.runtime_host.dispose().await;

        if let Some(resume) = format_resume_command(self.session_manager()) {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{} {resume}", crate::utils::chalk::dim("To resume this session:"));
            let _ = stdout.flush();
        }
        crate::utils::process_exit::exit(0);
    }

    pub(crate) async fn check_shutdown_requested(self: Arc<Self>) {
        if !self.state.lock().shutdown_requested {
            return;
        }
        self.shutdown(false).await;
    }

    fn register_signal_handlers(&self) {
        self.unregister_signal_handlers();
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::SeqCst);
        self.signal_generation.store(generation, Ordering::SeqCst);
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            for (kind, raw) in [(SignalKind::terminate(), libc::SIGTERM), (SignalKind::hangup(), libc::SIGHUP)] {
                let weak = self.weak.clone();
                let _guard = self.rt.enter();
                let Ok(mut stream) = signal(kind) else { continue };
                self.spawn(async move {
                    // One signal is handled; every path below ends the task.
                    if stream.recv().await.is_none() {
                        return;
                    }
                    let this = weak.upgrade();
                    let active = this.as_ref().map(|t| t.signal_generation.load(Ordering::SeqCst));
                    if active == Some(generation) {
                        // SIGHUP no longer hard-exits: graceful shutdown emits session_shutdown first,
                        // then attempts terminal restore.
                        crate::utils::shell::kill_tracked_detached_children();
                        if let Some(this) = this {
                            this.shutdown(true).await;
                        }
                        return;
                    }
                    if active.is_some_and(|g| g != 0) {
                        // A newer registration handles it.
                        return;
                    }
                    // Unregistered (TS removed its listener): tokio keeps the process handler
                    // installed, so restore the default action and re-raise the signal.
                    // SAFETY: resets the disposition and re-sends the signal to this process.
                    unsafe {
                        libc::signal(raw, libc::SIG_DFL);
                        libc::raise(raw);
                    }
                });
            }
        }

        // stdin needs the handler too: once the terminal is gone, reads fail with EIO (orphaned
        // background process group) or ENOTTY (revoked tty).
        let weak = self.weak.clone();
        pi_tui::terminal::set_stdin_error_handler(Some(Arc::new(move |error: &std::io::Error| {
            if let Some(this) = weak.upgrade()
                && this.signal_generation.load(Ordering::SeqCst) == generation
            {
                this.handle_terminal_error(error);
            }
        })));
        // stdout/stderr: once the terminal is gone, writes fail with EIO/EPIPE/ENOTCONN.
        let weak = self.weak.clone();
        pi_tui::terminal::set_output_error_handler(Some(Arc::new(move |error: &std::io::Error| {
            if let Some(this) = weak.upgrade()
                && this.signal_generation.load(Ordering::SeqCst) == generation
            {
                this.handle_terminal_error(error);
            }
        })));

        // Restore the terminal before the process dies on any uncaught panic (see
        // `utils::uncaught`). Without this, a panic leaves the terminal in raw mode with no cursor.
        let weak = self.weak.clone();
        crate::utils::uncaught::set_uncaught_handler(
            generation,
            Some(Arc::new(move |error: anyhow::Error| {
                if let Some(this) = weak.upgrade() {
                    this.uncaught_crash(&error);
                }
            })),
            // Panic messages would garble the TUI on the process terminal.
            self.options.terminal.is_none(),
        );
    }

    /// TS `unregisterSignalHandlers`: later SIGTERM/SIGHUP get their default action again.
    pub(crate) fn unregister_signal_handlers(&self) {
        let generation = self.signal_generation.swap(0, Ordering::SeqCst);
        if generation != 0 {
            pi_tui::terminal::set_stdin_error_handler(None);
            pi_tui::terminal::set_output_error_handler(None);
            crate::utils::uncaught::set_uncaught_handler(generation, None, false);
        }
    }

    /// TS `terminalErrorHandler` for stdin/stdout/stderr errors: a dead terminal exits with 129;
    /// anything else is rethrown and ends in the uncaught-exception handler.
    pub(crate) fn handle_terminal_error(&self, error: &std::io::Error) {
        if is_dead_terminal_error(error) {
            self.emergency_terminal_exit();
        }
        let error = match error.raw_os_error() {
            Some(code) => std::io::Error::from_raw_os_error(code),
            None => std::io::Error::new(error.kind(), error.to_string()),
        };
        // Reported off the stdin reader thread: restoring the terminal joins that thread.
        let this = self.this();
        let _ = std::thread::Builder::new()
            .name("pi-uncaught".into())
            .spawn(move || this.uncaught_crash(&anyhow::Error::new(error)));
    }

    pub(crate) fn suspend_signals(&self) -> Arc<dyn SuspendSignals> {
        let custom = self.state.lock().suspend_signals.clone();
        custom.unwrap_or_else(|| Arc::new(ProcessSuspendSignals::new(self.rt.clone())))
    }

    /// TS `handleCtrlZ`. An error is what TS throws; the key handler turns it into an uncaught crash.
    pub(crate) fn handle_ctrl_z(&self) -> std::io::Result<()> {
        if cfg!(windows) {
            self.show_status("Suspend to background is not supported on Windows");
            return Ok(());
        }
        let signals = self.suspend_signals();

        // Ignore SIGINT while suspended so Ctrl+C in the terminal does not kill the backgrounded
        // process. The handler is removed on resume.
        let restore_sigint: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>> =
            Arc::new(Mutex::new(Some(signals.ignore_sigint())));

        // Set up handler to restore TUI when resumed
        let cancel_sigcont = {
            let weak = self.weak.clone();
            let restore_sigint = restore_sigint.clone();
            signals.once_sigcont(Box::new(move || {
                if let Some(restore) = restore_sigint.lock().take() {
                    restore();
                }
                if let Some(this) = weak.upgrade() {
                    let ui = this.ui();
                    ui.start();
                    ui.request_render(true);
                }
            }))
        };

        // Stop the TUI (restore terminal to normal mode)
        self.ui().stop(pi_tui::tui::TuiStopOptions::default());

        // Send SIGTSTP to process group (pid=0 means all processes in group)
        if let Err(error) = signals.send_sigtstp() {
            cancel_sigcont();
            if let Some(restore) = restore_sigint.lock().take() {
                restore();
            }
            return Err(error);
        }
        Ok(())
    }

    /// TS `emergencyTerminalExit`: the terminal is gone, so skip the normal shutdown (its restore writes
    /// would fail again) and exit with 129.
    pub(crate) fn emergency_terminal_exit(&self) -> ! {
        self.state.lock().is_shutting_down = true;
        self.unregister_signal_handlers();
        crate::utils::shell::kill_tracked_detached_children();
        crate::utils::process_exit::exit(129);
    }

    /// TS `uncaughtCrash`: restore the terminal, report the error and the crash, and exit with 1.
    pub(crate) fn uncaught_crash(&self, error: &anyhow::Error) -> ! {
        // A dead terminal is not a pi crash. Do not try to restore it or record it.
        if error.downcast_ref::<std::io::Error>().is_some_and(is_dead_terminal_error) {
            self.emergency_terminal_exit();
        }
        if std::mem::replace(&mut self.state.lock().is_shutting_down, true) {
            crate::utils::process_exit::exit(1);
        }
        self.unregister_signal_handlers();
        crate::utils::shell::kill_tracked_detached_children();
        self.ui().stop(pi_tui::tui::TuiStopOptions::default());
        eprintln!("{APP_NAME} exiting due to uncaughtException:");
        // `console.error(error)` prints the stack; a panic's stack is where it happened.
        match error.downcast_ref::<crate::utils::uncaught::PanicError>() {
            Some(panic) => eprintln!("{}", panic.stack),
            None => eprintln!("{error:?}"),
        }
        if let Some(hint) = self.get_crash_extension_hint(error) {
            eprintln!("\n{hint}");
        }
        if self.record_crash(crate::core::crash_log::CrashRecordKind::UncaughtException, error) {
            eprintln!("\n{}", self.crash_report_instructions());
        }
        crate::utils::process_exit::exit(1);
    }

    pub(crate) async fn handle_follow_up(self: Arc<Self>) {
        let editor = self.editor();
        let text = editor.get_expanded_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let session = self.session();
        // Queue input during compaction (extension commands execute immediately)
        if session.is_compacting() {
            if self.is_extension_command(&text) {
                editor.add_to_history(&text);
                editor.set_text("");
                if let Err(error) = session.prompt(&text, None).await {
                    self.show_error(&err_message(&error));
                }
            } else {
                self.queue_compaction_message(&text, CompactionQueuedMessageMode::FollowUp);
            }
            return;
        }
        if session.is_streaming() {
            editor.add_to_history(&text);
            editor.set_text("");
            let options =
                PromptOptions { streaming_behavior: Some(PromptOptionsStreamingBehavior::FollowUp), ..Default::default() };
            if let Err(error) = session.prompt(&text, Some(&options)).await {
                self.show_error(&err_message(&error));
            }
            self.update_pending_messages_display();
            self.request_render();
        } else {
            // If not streaming, Alt+Enter acts like regular Enter
            editor.set_text("");
            self.clone().handle_submit(text);
        }
    }

    pub(crate) fn handle_dequeue(&self) {
        let restored = self.restore_queued_messages_to_editor(false, None);
        if restored == 0 {
            self.show_status("No queued messages to restore");
        } else {
            self.show_status(&format!(
                "Restored {restored} queued message{} to editor",
                if restored > 1 { "s" } else { "" }
            ));
        }
    }

    pub(crate) fn update_editor_border_color(&self) {
        let color: StyleFn = if self.state.lock().is_bash_mode {
            let f = theme().get_bash_mode_border_color();
            Arc::new(move |s: &str| f(s.to_string()))
        } else {
            let f = theme().get_thinking_border_color(self.session().thinking_level());
            Arc::new(move |s: &str| f(s.to_string()))
        };
        let (editor, indicator) = {
            let mut s = self.state.lock();
            s.editor_border_color = Some(color.clone());
            (s.editor.clone(), s.active_status_indicator.clone())
        };
        editor.set_border_color(color);
        if let Some(indicator) = indicator {
            indicator.invalidate();
        }
        self.request_render();
    }

    pub(crate) fn cycle_thinking_level(&self) {
        match self.session().cycle_thinking_level(None) {
            None => self.show_status("Current model does not support thinking"),
            Some(level) => {
                self.footer.invalidate();
                self.update_editor_border_color();
                self.show_status(&format!("Thinking level: {}", level.as_str()));
            }
        }
    }

    pub(crate) async fn cycle_model(self: Arc<Self>, direction: crate::core::agent_session::AgentSessionCycleModelDirection) {
        let session = self.session();
        match session.cycle_model(Some(direction), None).await {
            Ok(None) => {
                let msg = if !session.scoped_models().is_empty() { "Only one model in scope" } else { "Only one model available" };
                self.show_status(msg);
            }
            Ok(Some(result)) => {
                self.footer.invalidate();
                self.update_editor_border_color();
                let thinking = if result.model.reasoning && result.thinking_level != pi_agent::types::ThinkingLevel::Off {
                    format!(" (thinking: {})", result.thinking_level.as_str())
                } else {
                    String::new()
                };
                let name = if result.model.name.is_empty() { result.model.id.clone() } else { result.model.name.clone() };
                self.show_status(&format!("Switched to {name}{thinking}"));
                let this = self.clone();
                let model = result.model.clone();
                self.spawn(async move { this.maybe_warn_about_anthropic_subscription_auth(Some(model)).await });
            }
            Err(error) => self.show_error(&err_message(&error)),
        }
    }

    pub(crate) fn toggle_tool_output_expansion(&self) {
        let expanded = !self.state.lock().tool_output_expanded;
        self.set_tools_expanded(expanded);
    }

    pub(crate) fn set_tools_expanded(&self, expanded: bool) {
        let header = {
            let mut s = self.state.lock();
            if s.tool_output_expanded == expanded {
                return;
            }
            s.tool_output_expanded = expanded;
            s.custom_header.clone().or_else(|| s.built_in_header.clone())
        };
        if let Some(header) = header {
            set_expanded_on(&header, expanded);
        }
        for container in [&self.loaded_resources_container, &self.chat_container] {
            for child in container.children() {
                set_expanded_on(&child, expanded);
            }
        }
        self.show_status(&format!("Tool output: {}", if expanded { "expanded" } else { "collapsed" }));
    }

    /// Update rendered assistant messages without rebuilding live tool components.
    pub(crate) fn update_thinking_block_visibility(&self) {
        let hide = self.state.lock().hide_thinking_block;
        for child in self.chat_container.children() {
            if let Some(component) = pi_tui::tui::downcast_component::<AssistantMessageComponent>(&child) {
                component.set_hide_thinking_block(hide);
            }
        }
        self.request_render();
    }

    pub(crate) fn toggle_thinking_block_visibility(&self) {
        let hide = {
            let mut s = self.state.lock();
            s.hide_thinking_block = !s.hide_thinking_block;
            s.hide_thinking_block
        };
        self.settings_manager().set_hide_thinking_block(hide);
        self.update_thinking_block_visibility();
        self.show_status(&format!("Thinking blocks: {}", if hide { "hidden" } else { "visible" }));
    }

    pub(crate) async fn handle_open_external_editor(self: Arc<Self>) {
        let command = self.settings_manager().get_external_editor_command();
        let editor = self.editor();
        let content = editor.get_expanded_text();
        let ui = self.ui();
        ui.stop(pi_tui::tui::TuiStopOptions::default());
        let result = crate::modes::interactive::external_editor::edit_in_external_editor(
            &crate::modes::interactive::external_editor::ExternalEditorOptions { command, content },
        )
        .await;
        if let Ok(crate::modes::interactive::external_editor::ExternalEditorResult::Complete { content }) = result {
            editor.set_text(&content);
        }
        ui.start();
        ui.request_render(true);
    }

    // ------------------------------------------------------------------
    // UI helpers
    // ------------------------------------------------------------------

    pub(crate) fn clear_editor(&self) {
        self.editor().set_text("");
        self.request_render();
    }

    pub(crate) fn show_error(&self, error_message: &str) {
        self.add_spacer();
        let message = format!("Error: {error_message}");
        let pad = self.output_pad();
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Error, &message)), pad, 0);
        self.request_render();
    }

    pub(crate) fn show_warning(&self, warning_message: &str) {
        self.add_spacer();
        let message = format!("Warning: {warning_message}");
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Warning, &message)), 1, 0);
        self.request_render();
    }

    pub(crate) fn show_new_version_notification(&self, release: &LatestPiRelease) {
        use crate::modes::interactive::components::dynamic_border::DynamicBorder;
        let version = release.version.clone();
        let update_instruction = move || {
            format!(
                "{}{}",
                theme().fg(&ThemeColor::Muted, &format!("New version {version} is available. Run ")),
                theme().fg(&ThemeColor::Accent, &format!("{APP_NAME} update"))
            )
        };
        let changelog_url = "https://pi.dev/changelog";
        let changelog_line = move || {
            let link = if pi_tui::terminal_image::get_capabilities().hyperlinks {
                pi_tui::terminal_image::hyperlink(&theme().fg(&ThemeColor::Accent, changelog_url), changelog_url)
            } else {
                theme().fg(&ThemeColor::Accent, changelog_url)
            };
            format!("{}{link}", theme().fg(&ThemeColor::Muted, "Changelog: "))
        };
        let note = release.note.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
        let warning_border: Arc<dyn Fn(String) -> String + Send + Sync> =
            Arc::new(|text: String| theme().fg(&ThemeColor::Warning, &text));

        self.add_spacer();
        self.chat_container.add_child(Arc::new(DynamicBorder::new(Some(warning_border.clone()))));
        self.add_text(
            &self.chat_container,
            Arc::new(move || {
                format!("{}\n{}", theme().bold(&theme().fg(&ThemeColor::Warning, "Update Available")), update_instruction())
            }),
            1,
            0,
        );
        if let Some(note) = note {
            self.add_spacer();
            let style = pi_tui::components::markdown::DefaultTextStyle {
                color: Some(Arc::new(|text: &str| theme().fg(&ThemeColor::Muted, text))),
                ..Default::default()
            };
            self.chat_container.add_child(Arc::new(pi_tui::components::markdown::Markdown::new(
                note,
                1,
                0,
                self.get_markdown_theme_with_settings(),
                Some(style),
                None,
            )));
            self.add_spacer();
        }
        self.add_text(&self.chat_container, Arc::new(changelog_line), 1, 0);
        self.chat_container.add_child(Arc::new(DynamicBorder::new(Some(warning_border))));
        self.request_render();
    }

    pub(crate) fn show_package_update_notification(&self, packages: &[String]) {
        use crate::modes::interactive::components::dynamic_border::DynamicBorder;
        let package_lines = packages.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
        let warning_border: Arc<dyn Fn(String) -> String + Send + Sync> =
            Arc::new(|text: String| theme().fg(&ThemeColor::Warning, &text));
        self.add_spacer();
        self.chat_container.add_child(Arc::new(DynamicBorder::new(Some(warning_border.clone()))));
        self.add_text(
            &self.chat_container,
            Arc::new(move || {
                format!(
                    "{}\n{}{}\n{}\n{package_lines}",
                    theme().bold(&theme().fg(&ThemeColor::Warning, "Package Updates Available")),
                    theme().fg(&ThemeColor::Muted, "Package updates are available. Run "),
                    theme().fg(&ThemeColor::Accent, &format!("{APP_NAME} update --extensions")),
                    theme().fg(&ThemeColor::Muted, "Packages:"),
                )
            }),
            1,
            0,
        );
        self.chat_container.add_child(Arc::new(DynamicBorder::new(Some(warning_border))));
        self.request_render();
    }

    // ------------------------------------------------------------------
    // Queues
    // ------------------------------------------------------------------

    /// Get all queued messages (session queue and compaction queue).
    pub(crate) fn get_all_queued_messages(&self) -> (Vec<String>, Vec<String>) {
        let session = self.session();
        let compaction = self.state.lock().compaction_queued_messages.clone();
        let mut steering = session.get_steering_messages();
        steering.extend(compaction.iter().filter(|m| m.mode == CompactionQueuedMessageMode::Steer).map(|m| m.text.clone()));
        let mut follow_up = session.get_follow_up_messages();
        follow_up
            .extend(compaction.iter().filter(|m| m.mode == CompactionQueuedMessageMode::FollowUp).map(|m| m.text.clone()));
        (steering, follow_up)
    }

    /// Clear all queued messages and return their contents.
    pub(crate) fn clear_all_queues(&self) -> (Vec<String>, Vec<String>) {
        let result = self.session().clear_queue();
        let compaction = std::mem::take(&mut self.state.lock().compaction_queued_messages);
        let mut steering = result.steering;
        steering.extend(compaction.iter().filter(|m| m.mode == CompactionQueuedMessageMode::Steer).map(|m| m.text.clone()));
        let mut follow_up = result.follow_up;
        follow_up
            .extend(compaction.iter().filter(|m| m.mode == CompactionQueuedMessageMode::FollowUp).map(|m| m.text.clone()));
        (steering, follow_up)
    }

    pub(crate) fn update_pending_messages_display(&self) {
        use pi_tui::components::truncated_text::TruncatedText;
        self.pending_messages_container.clear();
        let (steering, follow_up) = self.get_all_queued_messages();
        if steering.is_empty() && follow_up.is_empty() {
            return;
        }
        self.pending_messages_container.add_child(Arc::new(Spacer::new(1)));
        for message in steering {
            let text = theme().fg(&ThemeColor::Dim, &format!("Steering: {message}"));
            self.pending_messages_container.add_child(Arc::new(TruncatedText::new(text, 1, 0)));
        }
        for message in follow_up {
            let text = theme().fg(&ThemeColor::Dim, &format!("Follow-up: {message}"));
            self.pending_messages_container.add_child(Arc::new(TruncatedText::new(text, 1, 0)));
        }
        let dequeue_hint =
            crate::modes::interactive::components::keybinding_hints::key_display_text("app.message.dequeue".to_string());
        let hint = theme().fg(&ThemeColor::Dim, &format!("↳ {dequeue_hint} to edit all queued messages"));
        self.pending_messages_container.add_child(Arc::new(TruncatedText::new(hint, 1, 0)));
    }

    pub(crate) fn restore_queued_messages_to_editor(&self, abort: bool, current_text: Option<String>) -> usize {
        let (steering, follow_up) = self.clear_all_queues();
        let all: Vec<String> = steering.into_iter().chain(follow_up).collect();
        let session = self.session();
        let abort_session = |session: Arc<AgentSession>| {
            self.spawn(async move {
                let _ = session.abort().await;
            });
        };
        if all.is_empty() {
            self.update_pending_messages_display();
            if abort {
                abort_session(session);
            }
            return 0;
        }
        let queued_text = all.join("\n\n");
        let editor = self.editor();
        let current_text = current_text.unwrap_or_else(|| editor.get_text());
        let combined = [queued_text, current_text].into_iter().filter(|t| !t.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
        editor.set_text(&combined);
        self.update_pending_messages_display();
        if abort {
            abort_session(session);
        }
        all.len()
    }

    pub(crate) fn queue_compaction_message(&self, text: &str, mode: CompactionQueuedMessageMode) {
        self.state.lock().compaction_queued_messages.push(CompactionQueuedMessage { text: text.to_string(), mode });
        let editor = self.editor();
        editor.add_to_history(text);
        editor.set_text("");
        self.update_pending_messages_display();
        self.show_status("Queued message for after compaction");
    }

    pub(crate) fn is_extension_command(&self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else { return false };
        let command_name = match rest.find(' ') {
            Some(index) => &rest[..index],
            None => rest,
        };
        self.session().extension_runner().get_command(command_name).is_some()
    }

    pub(crate) async fn flush_compaction_queue(self: Arc<Self>, will_retry: bool) {
        let queued = {
            let mut s = self.state.lock();
            if s.compaction_queued_messages.is_empty() {
                return;
            }
            std::mem::take(&mut s.compaction_queued_messages)
        };
        self.update_pending_messages_display();
        let session = self.session();

        let restore_queue = |this: &Inner, error: &anyhow::Error, queued: &[CompactionQueuedMessage]| {
            this.session().clear_queue();
            this.state.lock().compaction_queued_messages = queued.to_vec();
            this.update_pending_messages_display();
            this.show_error(&format!(
                "Failed to send queued message{}: {}",
                if queued.len() > 1 { "s" } else { "" },
                err_message(error)
            ));
        };

        let send_queued = |message: CompactionQueuedMessage| {
            let session = session.clone();
            let is_command = self.is_extension_command(&message.text);
            async move {
                if is_command {
                    session.prompt(&message.text, None).await
                } else if message.mode == CompactionQueuedMessageMode::FollowUp {
                    session.follow_up(&message.text, None, None).await.map(|_| ())
                } else {
                    session.steer(&message.text, None, None).await.map(|_| ())
                }
            }
        };

        let result: anyhow::Result<()> = async {
            if will_retry {
                // When retry is pending, queue messages for the retry turn
                for message in queued.clone() {
                    send_queued(message).await?;
                }
                self.update_pending_messages_display();
                return Ok(());
            }

            // Find first non-extension-command message to use as prompt
            let first_prompt_index = queued.iter().position(|m| !self.is_extension_command(&m.text));
            let Some(first_prompt_index) = first_prompt_index else {
                // All extension commands - execute them all
                for message in &queued {
                    session.prompt(&message.text, None).await?;
                }
                return Ok(());
            };

            for message in &queued[..first_prompt_index] {
                session.prompt(&message.text, None).await?;
            }
            let first = queued[first_prompt_index].clone();
            let rest = queued[first_prompt_index + 1..].to_vec();

            // Start a prompt when idle, or queue it into a run still finishing compaction. The prompt
            // runs up to its first suspension point now (TS calls it without awaiting), so it starts
            // before the rest is queued.
            {
                let this = self.clone();
                let session = session.clone();
                let queued_all = queued.clone();
                self.start_now(async move {
                    let behavior = match first.mode {
                        CompactionQueuedMessageMode::Steer => PromptOptionsStreamingBehavior::Steer,
                        CompactionQueuedMessageMode::FollowUp => PromptOptionsStreamingBehavior::FollowUp,
                    };
                    let options = PromptOptions { streaming_behavior: Some(behavior), ..Default::default() };
                    if let Err(error) = session.prompt(&first.text, Some(&options)).await {
                        restore_queue(&this, &error, &queued_all);
                    }
                });
            }
            for message in rest {
                send_queued(message).await?;
            }
            self.update_pending_messages_display();
            Ok(())
        }
        .await;
        if let Err(error) = result {
            restore_queue(&self, &error, &queued);
        }
    }

    /// Move pending bash components from pending area to chat
    pub(crate) fn flush_pending_bash_components(&self) {
        let pending = std::mem::take(&mut self.state.lock().pending_bash_components);
        for component in pending {
            let component: ComponentRef = component;
            self.pending_messages_container.remove_child(&component);
            self.chat_container.add_child(component);
        }
    }

    pub(crate) fn update_available_provider_count(&self) {
        let session = self.session();
        let scoped = session.scoped_models();
        let providers: HashSet<String> = if !scoped.is_empty() {
            scoped.iter().map(|s| s.model.provider.to_string()).collect()
        } else {
            session.model_runtime().get_available_snapshot().iter().map(|m| m.provider.to_string()).collect()
        };
        self.footer_data_provider.set_available_provider_count(providers.len() as i64);
    }

    pub(crate) async fn maybe_warn_about_anthropic_subscription_auth(&self, model: Option<pi_ai::types::Model>) {
        let session = self.session();
        let model = model.or_else(|| session.model());
        if self.settings_manager().get_warnings().anthropic_extra_usage == Some(false) {
            return;
        }
        if self.state.lock().anthropic_subscription_warning_shown {
            return;
        }
        let Some(model) = model else { return };
        if model.provider != "anthropic" {
            return;
        }
        let runtime = session.model_runtime();
        if let Ok(Some(status)) = runtime.check_auth("anthropic", None).await
            && status.auth_type == pi_ai::auth::types::AuthType::OAuth
        {
            self.state.lock().anthropic_subscription_warning_shown = true;
            self.show_warning(ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
            return;
        }
        let auth = runtime
            .get_auth(
                &crate::core::model_runtime::ModelRuntimeGetAuthProviderOrModel::String(model.provider.to_string()),
                None,
            )
            .await;
        let api_key = auth.ok().flatten().and_then(|a| a.auth.api_key.clone());
        if !is_anthropic_subscription_auth_key(api_key.as_deref()) {
            return;
        }
        self.state.lock().anthropic_subscription_warning_shown = true;
        self.show_warning(ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
    }

    pub(crate) fn stop(&self, fullscreen_exit_output: Option<FullscreenExitOutput>) {
        let fullscreen_exit_output = fullscreen_exit_output.unwrap_or_else(|| self.settings_manager().get_fullscreen_exit_output());
        self.dispose_active_selector();
        if self.settings_manager().get_show_terminal_progress() {
            self.ui().terminal().set_progress(false);
        }
        self.clear_status_indicator(None);
        self.theme_controller.disable_auto_sync();
        self.clear_extension_terminal_input_listeners();
        self.footer.dispose();
        self.footer_data_provider.dispose();
        let unsubscribe = self.state.lock().unsubscribe.take();
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe();
        }
        let initialized = std::mem::replace(&mut self.state.lock().is_initialized, false);
        if initialized {
            self.stop_interactive_tui(fullscreen_exit_output);
        }
        self.unregister_signal_handlers();
    }
}

/// Set the expansion state of a component that supports it.
pub(crate) fn set_expanded_on(component: &ComponentRef, expanded: bool) {
    use crate::modes::interactive::components::{
        branch_summary_message::BranchSummaryMessageComponent, compaction_summary_message::CompactionSummaryMessageComponent,
        custom_entry::CustomEntryComponent, custom_message::CustomMessageComponent,
        skill_invocation_message::SkillInvocationMessageComponent,
    };
    use pi_tui::tui::downcast_component as dc;
    if let Some(c) = dc::<ExpandableText>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<BuiltInHeader>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<ToolExecutionComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<BashExecutionComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<CustomMessageComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<CustomEntryComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<BranchSummaryMessageComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<CompactionSummaryMessageComponent>(component) {
        c.set_expanded(expanded);
    } else if let Some(c) = dc::<SkillInvocationMessageComponent>(component) {
        c.set_expanded(expanded);
    }
}

/// Set the output padding of a component that supports it.
pub(crate) fn set_output_pad_on(component: &ComponentRef, padding: i64) {
    use crate::modes::interactive::components::{
        branch_summary_message::BranchSummaryMessageComponent, compaction_summary_message::CompactionSummaryMessageComponent,
        custom_entry::CustomEntryComponent, custom_message::CustomMessageComponent,
        skill_invocation_message::SkillInvocationMessageComponent, user_message::UserMessageComponent,
    };
    use pi_tui::tui::downcast_component as dc;
    if let Some(c) = dc::<AssistantMessageComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<ToolExecutionComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<BashExecutionComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<UserMessageComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<CustomMessageComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<CustomEntryComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<BranchSummaryMessageComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<CompactionSummaryMessageComponent>(component) {
        c.set_output_pad(padding);
    } else if let Some(c) = dc::<SkillInvocationMessageComponent>(component) {
        c.set_output_pad(padding);
    }
}

fn url_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20")
}

/// Route pi-tui event-loop futures (copy-to-clipboard, async component work) to tokio.
fn install_future_spawner(rt: &tokio::runtime::Handle) {
    let rt = rt.clone();
    pi_tui::event_loop::set_future_spawner(Some(Arc::new(move |future| {
        rt.spawn(crate::utils::uncaught::supervise(future));
    })));
}

async fn check_tmux_keyboard_setup() -> Option<String> {
    std::env::var_os("TMUX")?;
    async fn run_tmux_show(option: &str) -> Option<String> {
        let child = tokio::process::Command::new("tmux")
            .args(["show", "-gv", option])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(std::time::Duration::from_secs(2), child).await {
            Ok(Ok(output)) if output.status.success() => Some(String::from_utf8_lossy(&output.stdout).trim().to_string()),
            _ => None,
        }
    }
    let (extended_keys, extended_keys_format) =
        tokio::join!(run_tmux_show("extended-keys"), run_tmux_show("extended-keys-format"));
    // If we couldn't query tmux (timeout, sandbox, etc.), don't warn
    let extended_keys = extended_keys?;
    if extended_keys != "on" && extended_keys != "always" {
        return Some(
            "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux."
                .to_string(),
        );
    }
    if extended_keys_format.as_deref() == Some("xterm") {
        return Some(
            "tmux extended-keys-format is xterm. Pi works best with csi-u. Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux."
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_crash_extension_hints() {
        assert_eq!(format_crash_extension_hint(None), None);
        assert_eq!(format_crash_extension_hint(Some(&[String::new()])), None);
        assert_eq!(
            format_crash_extension_hint(Some(&["a".to_string()])).unwrap(),
            "A stack frame came from loaded extension `a`, which may be involved. Try disabling it with `pi config`, or run `pi -ne` to confirm."
        );
        assert!(format_crash_extension_hint(Some(&["a".into(), "b".into()])).unwrap().contains("`a` and `b`"));
        assert!(
            format_crash_extension_hint(Some(&["a".into(), "b".into(), "c".into()]))
                .unwrap()
                .contains("extensions `a`, `b`, and `c`, which may be involved. Try disabling them")
        );
    }

    #[test]
    fn quotes_shell_arguments_when_needed() {
        assert_eq!(quote_if_needed("/tmp/sessions"), "/tmp/sessions");
        assert_eq!(quote_if_needed("/tmp/my sessions"), "'/tmp/my sessions'");
        assert_eq!(quote_if_needed("it's"), "'it'\\''s'");
        assert_eq!(quote_if_needed(""), "''");
    }
}
