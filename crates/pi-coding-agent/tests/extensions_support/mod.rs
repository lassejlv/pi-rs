//! Shared helpers for the extension runner / loader / JS bridge tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use pi_coding_agent::core::auth_storage::AuthStorage;
use pi_coding_agent::core::extensions::types::*;
use pi_coding_agent::core::model_registry::ModelRegistry;
use pi_coding_agent::core::model_resolver::ScopedModel;
use pi_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pi_coding_agent::core::session_manager::SessionManager;
use pi_coding_agent::core::settings_manager::Settings;
use pi_coding_agent::core::slash_commands::SlashCommandInfo;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_SOURCE: &str =
    "/tmp/claude-0/-home-user-pi-rs/3420df64-2251-57a4-b9aa-4eb6ba35ed96/scratchpad/src/pi";

/// `packages/coding-agent/examples/extensions` of the TS reference checkout (`PI_TS_SOURCE`).
pub fn examples_dir() -> Option<PathBuf> {
    let root = std::env::var("PI_TS_SOURCE").unwrap_or_else(|_| DEFAULT_SOURCE.to_string());
    let dir = Path::new(&root).join("packages/coding-agent/examples/extensions");
    dir.is_dir().then_some(dir)
}

pub async fn model_registry() -> Arc<ModelRegistry> {
    let options = CreateModelRuntimeOptions {
        credentials: Some(AuthStorage::in_memory(None)),
        models_path: Some(String::new()),
        allow_model_network: Some(false),
        ..Default::default()
    };
    let runtime: Arc<ModelRuntime> = ModelRuntime::create(Some(&options))
        .await
        .expect("model runtime");
    Arc::new(ModelRegistry::new(runtime))
}

pub fn session_manager(cwd: &str) -> Arc<SessionManager> {
    SessionManager::in_memory(Some(cwd), None, None).expect("session manager")
}

/// Records what extensions do through `pi.*` actions.
#[derive(Default)]
pub struct RecordingActions {
    pub sent: Mutex<Vec<Value>>,
    pub user_messages: Mutex<Vec<Value>>,
    pub entries: Mutex<Vec<(String, Option<Value>)>>,
    pub active_tools: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ExtensionActions for RecordingActions {
    fn send_message(
        &self,
        message: CustomMessageInput,
        options: SendMessageOptions,
    ) -> anyhow::Result<()> {
        self.sent
            .lock()
            .push(serde_json::json!({ "message": message, "options": options }));
        Ok(())
    }
    fn send_user_message(
        &self,
        content: pi_ai::types::UserContent,
        options: SendUserMessageOptions,
    ) -> anyhow::Result<()> {
        self.user_messages
            .lock()
            .push(serde_json::json!({ "content": content, "options": options }));
        Ok(())
    }
    fn append_entry(&self, custom_type: &str, data: Option<Value>) -> anyhow::Result<()> {
        self.entries.lock().push((custom_type.to_string(), data));
        Ok(())
    }
    fn set_session_name(&self, _name: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn get_session_name(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn set_label(&self, _entry_id: &str, _label: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn get_active_tools(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.active_tools.lock().clone())
    }
    fn get_all_tools(&self) -> anyhow::Result<Vec<ToolInfo>> {
        Ok(Vec::new())
    }
    fn get_settings(&self) -> anyhow::Result<Settings> {
        Ok(Settings::default())
    }
    fn set_active_tools(&self, tool_names: Vec<String>) -> anyhow::Result<()> {
        *self.active_tools.lock() = tool_names;
        Ok(())
    }
    fn refresh_tools(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn get_commands(&self) -> anyhow::Result<Vec<SlashCommandInfo>> {
        Ok(Vec::new())
    }
    async fn set_model(&self, _model: pi_ai::types::Model) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn get_thinking_level(&self) -> anyhow::Result<pi_agent::types::ThinkingLevel> {
        Ok(pi_agent::types::ThinkingLevel::Off)
    }
    fn set_thinking_level(&self, _level: pi_agent::types::ThinkingLevel) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Context actions with the TS test defaults, configurable per test.
pub struct TestContextActions {
    pub signal: Mutex<Option<CancellationToken>>,
    pub project_trusted: bool,
    pub scoped_models: Vec<ScopedModel>,
}

impl Default for TestContextActions {
    fn default() -> Self {
        Self {
            signal: Mutex::new(None),
            project_trusted: true,
            scoped_models: Vec::new(),
        }
    }
}

impl ExtensionContextActions for TestContextActions {
    fn get_model(&self) -> Option<pi_ai::types::Model> {
        None
    }
    fn get_scoped_models(&self) -> Vec<ScopedModel> {
        self.scoped_models.clone()
    }
    fn is_idle(&self) -> bool {
        true
    }
    fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }
    fn get_signal(&self) -> Option<CancellationToken> {
        self.signal.lock().clone()
    }
    fn abort(&self) {}
    fn has_pending_messages(&self) -> bool {
        false
    }
    fn shutdown(&self) {}
    fn get_context_usage(&self) -> Option<ContextUsage> {
        None
    }
    fn compact(&self, _options: CompactOptions) {}
    fn get_system_prompt(&self) -> String {
        String::new()
    }
}

/// UI context for tests: scripted dialog answers, recorded notifications; everything else is
/// the no-op context.
#[derive(Default)]
pub struct TestUi {
    pub select_answer: Mutex<Option<String>>,
    pub confirm_answer: Mutex<bool>,
    pub selects: Mutex<Vec<(String, Vec<String>)>>,
    pub notifications: Mutex<Vec<String>>,
    pub statuses: Mutex<Vec<(String, Option<String>)>>,
}

use pi_coding_agent::core::extensions::runner::NoOpUIContext;
use pi_coding_agent::modes::interactive::theme::theme::Theme;

#[async_trait::async_trait]
impl ExtensionUIContext for TestUi {
    async fn select(
        &self,
        title: &str,
        options: Vec<String>,
        _opts: ExtensionUIDialogOptions,
    ) -> Option<String> {
        self.selects.lock().push((title.to_string(), options));
        self.select_answer.lock().clone()
    }
    async fn confirm(&self, _title: &str, _message: &str, _opts: ExtensionUIDialogOptions) -> bool {
        *self.confirm_answer.lock()
    }
    async fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _opts: ExtensionUIDialogOptions,
    ) -> Option<String> {
        None
    }
    fn notify(&self, message: &str, _kind: NotifyType) {
        self.notifications.lock().push(message.to_string());
    }
    fn on_terminal_input(&self, handler: TerminalInputHandler) -> Unsubscribe {
        NoOpUIContext.on_terminal_input(handler)
    }
    fn set_status(&self, key: &str, text: Option<&str>) {
        self.statuses
            .lock()
            .push((key.to_string(), text.map(str::to_string)));
    }
    fn set_working_message(&self, _message: Option<&str>) {}
    fn set_working_visible(&self, _visible: bool) {}
    fn set_working_indicator(&self, _options: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}
    fn set_widget(
        &self,
        _key: &str,
        _content: Option<WidgetContent>,
        _options: ExtensionWidgetOptions,
    ) {
    }
    fn set_footer(&self, _factory: Option<FooterFactory>) {}
    fn set_header(&self, _factory: Option<ComponentFactory>) {}
    fn set_title(&self, _title: &str) {}
    async fn custom(
        &self,
        factory: CustomComponentFactory,
        options: CustomUiOptions,
    ) -> anyhow::Result<CustomUiResult> {
        NoOpUIContext.custom(factory, options).await
    }
    fn paste_to_editor(&self, _text: &str) {}
    fn set_editor_text(&self, _text: &str) {}
    fn get_editor_text(&self) -> String {
        String::new()
    }
    async fn editor(&self, _title: &str, _prefill: Option<&str>) -> Option<String> {
        None
    }
    fn add_autocomplete_provider(&self, _factory: AutocompleteProviderFactory) {}
    fn set_editor_component(&self, _factory: Option<EditorFactory>) {}
    fn get_editor_component(&self) -> Option<EditorFactory> {
        None
    }
    fn theme(&self) -> Arc<Theme> {
        NoOpUIContext.theme()
    }
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }
    fn get_theme(&self, _name: &str) -> Option<Arc<Theme>> {
        None
    }
    fn set_theme(&self, theme: ThemeSelection) -> SetThemeResult {
        NoOpUIContext.set_theme(theme)
    }
    fn get_tools_expanded(&self) -> bool {
        false
    }
    fn set_tools_expanded(&self, _expanded: bool) {}
}

/// Temp dir with an `extensions/` subdirectory, used as both cwd and agent dir (TS test setup).
pub struct ExtDir {
    pub tmp: tempfile::TempDir,
}

impl ExtDir {
    pub fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("pi-ext-test-")
            .tempdir()
            .unwrap();
        std::fs::create_dir(tmp.path().join("extensions")).unwrap();
        Self { tmp }
    }
    pub fn root(&self) -> String {
        self.tmp.path().display().to_string()
    }
    pub fn ext_dir(&self) -> PathBuf {
        self.tmp.path().join("extensions")
    }
    /// Write `extensions/<rel>` (creating parent directories) and return its path.
    pub fn write(&self, rel: &str, code: &str) -> String {
        let path = self.ext_dir().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, code).unwrap();
        path.display().to_string()
    }
    pub async fn discover(&self) -> LoadExtensionsResult {
        self.discover_with(&[], None).await
    }
    pub async fn discover_with(
        &self,
        configured: &[String],
        bus: Option<Arc<dyn pi_coding_agent::core::event_bus::EventBus>>,
    ) -> LoadExtensionsResult {
        let root = self.root();
        pi_coding_agent::core::extensions::loader::discover_and_load_extensions(
            configured,
            &root,
            Some(&root),
            bus,
        )
        .await
        .unwrap()
    }
    pub async fn runner(
        &self,
        result: &LoadExtensionsResult,
    ) -> Arc<pi_coding_agent::core::extensions::runner::ExtensionRunner> {
        let root = self.root();
        pi_coding_agent::core::extensions::runner::ExtensionRunner::new(
            &result.extensions,
            result.runtime.clone(),
            &root,
            session_manager(&root),
            model_registry().await,
        )
    }
}

/// Collects error messages reported by a runner.
pub fn collect_errors(
    runner: &pi_coding_agent::core::extensions::runner::ExtensionRunner,
) -> Arc<Mutex<Vec<ExtensionError>>> {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    runner.on_error(Arc::new(move |e| sink.lock().push(e)));
    errors
}

/// Wait (up to ~2 s) until `cond` holds: void UI calls from JS are delivered asynchronously,
/// in order, on the bridge's UI executor.
pub async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A local HTTP server answering every request with an Anthropic Messages SSE stream whose text
/// is `reply`. Returns the base URL and the received requests (request line, headers and body).
pub async fn mock_anthropic_server(reply: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    let text = serde_json::to_string(reply).unwrap();
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"mock","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#.to_string(),
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
        format!(r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{text}}}}}"#),
        r#"{"type":"content_block_stop","index":0}"#.to_string(),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#.to_string(),
        r#"{"type":"message_stop"}"#.to_string(),
    ];
    let body: String = events
        .iter()
        .map(|data| {
            let kind: Value = serde_json::from_str(data).unwrap();
            format!(
                "event: {}\ndata: {data}\n\n",
                kind["type"].as_str().unwrap()
            )
        })
        .collect();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Read headers, then the body by Content-Length.
                loop {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            seen.lock().push(text);
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base_url, requests)
}
