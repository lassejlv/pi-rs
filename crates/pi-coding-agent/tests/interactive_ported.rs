//! Ports of `interactive-mode-*.test.ts` cases that can run against the real `InteractiveMode` on
//! pi-tui's `VirtualTerminal` with the faux provider. The TypeScript tests call private methods on
//! fake `this` objects; these drive the same code paths end to end and assert on the screen:
//!
//! - `interactive-mode-extension-dialogs.test.ts`: stacked confirms, aborted hidden dialogs, stacked
//!   inline custom components, a confirm opened from a custom component.
//! - `interactive-mode-anthropic-warning.test.ts`: subscription auth warnings.
//! - `interactive-mode-assistant-diagnostics.test.ts`: Anthropic thinking-drop notices.
//! - `interactive-mode-bug-report-hint.test.ts`: `/bug` hints after errors.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pi_ai::auth::types::{CredentialStore, Credential};
use pi_ai::providers::faux::{FauxModelDefinition, FauxResponseStep};
use pi_ai::types::AssistantMessage;
use pi_coding_agent::core::agent_session_runtime::{AgentSessionRuntime, CreateAgentSessionRuntimeFactory};
use pi_coding_agent::core::agent_session_services::AgentSessionServices;
use pi_coding_agent::core::extensions::types::{
    CommandOptions, CustomUiOptions, CustomUiResult, ExtensionAPIRef, ExtensionFactory, ExtensionUIContext,
    ExtensionUIDialogOptions, custom_typed,
};
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions, InteractiveModeTestHooks,
};
use pi_coding_agent::core::settings_schema::TuiMode;
use std::sync::atomic::{AtomicUsize, Ordering};
use pi_tui::components::text::Text;
use pi_tui::terminal::Terminal;
use pi_tui::testing::VirtualTerminal;
use pi_tui::tui::{Component, ComponentRef};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ESC: &str = "\x1b";
const ENTER: &str = "\r";

type UiSlot = Arc<Mutex<Option<Arc<dyn ExtensionUIContext>>>>;

/// An extension with a `/capture-ui` command that hands the command context's UI to the test.
fn capture_ui_extension(slot: UiSlot) -> TestExtension {
    let factory: ExtensionFactory = Arc::new(move |pi: ExtensionAPIRef| {
        let slot = slot.clone();
        Box::pin(async move {
            pi.register_command(
                "capture-ui",
                CommandOptions {
                    description: Some("Capture the UI context".into()),
                    get_argument_completions: None,
                    handler: Arc::new(move |_args, ctx| {
                        let slot = slot.clone();
                        Box::pin(async move {
                            *slot.lock() = Some(ctx.ui()?);
                            Ok(())
                        })
                    }),
                },
            )?;
            Ok(())
        })
    });
    TestExtension::new(factory)
}

#[derive(Default)]
struct FixtureOptions {
    provider: Option<String>,
    models: Vec<FauxModelDefinition>,
    settings: Option<Value>,
    extensions: Vec<TestExtension>,
    credential: Option<Value>,
    tui_mode: Option<TuiMode>,
    /// Leave `InteractiveMode::init` to the test.
    skip_init: bool,
    /// Terminal size (default 100x40).
    size: Option<(usize, usize)>,
}

/// A `VirtualTerminal` that counts `start`/`stop` calls and records progress updates (TS `RecordingTerminal`).
struct RecordingTerminal {
    inner: VirtualTerminal,
    starts: AtomicUsize,
    stops: AtomicUsize,
    progress: Mutex<Vec<bool>>,
}

impl RecordingTerminal {
    fn counts(&self) -> (usize, usize) {
        (self.starts.load(Ordering::SeqCst), self.stops.load(Ordering::SeqCst))
    }
}

impl Terminal for RecordingTerminal {
    fn start(&self, on_input: pi_tui::terminal::InputHandler, on_resize: pi_tui::terminal::ResizeHandler) {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.inner.start(on_input, on_resize);
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.inner.stop();
    }
    fn write(&self, data: &str) {
        self.inner.write(data);
    }
    fn columns(&self) -> usize {
        self.inner.columns()
    }
    fn rows(&self) -> usize {
        self.inner.rows()
    }
    fn kitty_protocol_active(&self) -> bool {
        self.inner.kitty_protocol_active()
    }
    fn move_by(&self, lines: i64) {
        self.inner.move_by(lines);
    }
    fn hide_cursor(&self) {
        self.inner.hide_cursor();
    }
    fn show_cursor(&self) {
        self.inner.show_cursor();
    }
    fn clear_line(&self) {
        self.inner.clear_line();
    }
    fn clear_from_cursor(&self) {
        self.inner.clear_from_cursor();
    }
    fn clear_screen(&self) {
        self.inner.clear_screen();
    }
    fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }
    fn set_progress(&self, active: bool) {
        self.progress.lock().push(active);
    }
}

struct Fixture {
    harness: Harness,
    mode: InteractiveMode,
    terminal: VirtualTerminal,
    recording: Arc<RecordingTerminal>,
    agent_dir: std::path::PathBuf,
    run: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Fixture {
    async fn new(options: FixtureOptions) -> Fixture {
        let agent_dir = std::env::temp_dir().join(format!("pi-interactive-ported-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&agent_dir).unwrap();
        // SAFETY: tests in this binary are serialized by LOCK.
        unsafe {
            std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);
            std::env::set_var("PI_OFFLINE", "1");
            std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
        }
        let mut settings = json!({ "quietStartup": true, "retry": { "enabled": false } });
        if let Some(Value::Object(extra)) = options.settings {
            for (k, v) in extra {
                settings[k] = v;
            }
        }
        let harness = create_harness(HarnessOptions {
            provider: options.provider,
            models: options.models,
            settings: Some(settings),
            extension_factories: options.extensions,
            ..Default::default()
        })
        .await
        .unwrap();
        if let Some(credential) = options.credential {
            let provider = harness.get_model().provider.to_string();
            let credential: Credential = serde_json::from_value(credential).unwrap();
            harness
                .auth_storage
                .modify(&provider, Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })), None)
                .await
                .unwrap();
        }
        let session = harness.session.clone();
        let services = AgentSessionServices {
            cwd: harness.session_manager.get_cwd(),
            agent_dir: agent_dir.to_string_lossy().into_owned(),
            model_runtime: harness.model_runtime.clone(),
            settings_manager: harness.settings_manager.clone(),
            resource_loader: session.resource_loader(),
            diagnostics: Vec::new(),
        };
        let factory: CreateAgentSessionRuntimeFactory =
            Arc::new(|_| Box::pin(async { Err(anyhow::anyhow!("session replacement is not supported in this test")) }));
        let runtime = Arc::new(AgentSessionRuntime::new(session, &services, &factory, None, None));
        let (columns, rows) = options.size.unwrap_or((100, 40));
        let terminal = VirtualTerminal::new(columns, rows);
        let recording = Arc::new(RecordingTerminal {
            inner: terminal.clone(),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            progress: Mutex::new(Vec::new()),
        });
        let mode = InteractiveMode::new(
            runtime,
            Some(&InteractiveModeOptions {
                terminal: Some(recording.clone() as Arc<dyn Terminal>),
                tui_mode: options.tui_mode,
                ..Default::default()
            }),
        );
        let fx = Fixture { harness, mode, terminal, recording, agent_dir, run: None };
        if !options.skip_init {
            fx.mode.init().await.unwrap();
            fx.wait_for("the editor", |s| s.contains('─')).await;
        }
        fx
    }

    fn hooks(&self) -> InteractiveModeTestHooks {
        self.mode.test_hooks()
    }

    /// The chat transcript, without ANSI styling.
    fn chat(&self) -> String {
        strip_ansi(&self.hooks().render_chat(100).join("\n"))
    }

    /// Start the main loop (`InteractiveMode::run`), which handles startup warnings and prompts.
    async fn start(&mut self) {
        let mode = self.mode.clone();
        self.run = Some(tokio::spawn(async move { mode.run().await }));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    fn screen(&self) -> String {
        self.terminal.get_scroll_buffer().join("\n")
    }

    fn viewport(&self) -> String {
        self.terminal.get_viewport().join("\n")
    }

    async fn wait_for(&self, what: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.mode.tui().request_render(false);
            tokio::time::sleep(Duration::from_millis(30)).await;
            let screen = self.screen();
            if predicate(&screen) {
                return screen;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}; screen:\n{screen}");
            }
        }
    }

    /// Let pending renders and spawned tasks settle.
    async fn settle(&self) -> String {
        for _ in 0..5 {
            self.mode.tui().request_render(false);
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        self.screen()
    }

    async fn type_text(&self, text: &str) {
        for ch in text.chars() {
            self.terminal.send_input(&ch.to_string());
        }
        self.terminal.send_input(ENTER);
    }

    async fn capture_ui(&mut self, slot: &UiSlot) -> Arc<dyn ExtensionUIContext> {
        if self.run.is_none() {
            self.start().await;
        }
        self.mode.submit("/capture-ui");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(ui) = slot.lock().clone() {
                return ui;
            }
            assert!(Instant::now() < deadline, "the capture command did not run:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn cleanup(self) {
        if let Some(run) = self.run {
            run.abort();
        }
        self.mode.stop(None);
        self.harness.cleanup();
        let _ = std::fs::remove_dir_all(&self.agent_dir);
    }
}

fn spawn_confirm(
    ui: &Arc<dyn ExtensionUIContext>,
    title: &'static str,
    message: &'static str,
    signal: Option<CancellationToken>,
) -> tokio::task::JoinHandle<bool> {
    let ui = ui.clone();
    tokio::spawn(async move { ui.confirm(title, message, ExtensionUIDialogOptions { signal, timeout: None }).await })
}

async fn resolved<T>(handle: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), handle).await.expect("dialog did not resolve").unwrap()
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-extension-dialogs.test.ts
// -------------------------------------------------------------------------------------------------

// Regression for #11166: a second dialog replaced the first one and the first promise never settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shows_the_earlier_confirm_again_after_the_later_one_is_answered() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions { extensions: vec![capture_ui_extension(slot.clone())], ..Default::default() }).await;
    let ui = fx.capture_ui(&slot).await;

    let first = spawn_confirm(&ui, "First", "first dialog", None);
    fx.wait_for("first", |_| true).await;
    let second = spawn_confirm(&ui, "Second", "second dialog", None);
    fx.wait_for("the second dialog", |s| s.contains("second dialog")).await;
    assert!(!fx.viewport().contains("first dialog"), "{}", fx.viewport());

    fx.terminal.send_input(ESC);
    assert!(!resolved(second).await);
    fx.wait_for("the first dialog again", |_| fx.viewport().contains("first dialog")).await;

    fx.terminal.send_input(ENTER);
    assert!(resolved(first).await);
    let screen = fx.settle().await;
    assert!(!fx.viewport().contains("first dialog"), "{screen}");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removes_a_hidden_dialog_when_its_signal_aborts() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions { extensions: vec![capture_ui_extension(slot.clone())], ..Default::default() }).await;
    let ui = fx.capture_ui(&slot).await;

    let controller = CancellationToken::new();
    let first = spawn_confirm(&ui, "First", "first dialog", Some(controller.clone()));
    fx.wait_for("first", |_| true).await;
    let second = spawn_confirm(&ui, "Second", "second dialog", None);
    fx.wait_for("the second dialog", |_| fx.viewport().contains("second dialog")).await;

    controller.cancel();
    assert!(!resolved(first).await);
    fx.settle().await;
    assert!(fx.viewport().contains("second dialog"), "{}", fx.viewport());

    fx.terminal.send_input(ENTER);
    assert!(resolved(second).await);
    fx.settle().await;
    assert!(!fx.viewport().contains("dialog"), "{}", fx.viewport());
    fx.cleanup();
}

/// A custom component whose `done` callback the test keeps.
fn open_custom(
    ui: &Arc<dyn ExtensionUIContext>,
    label: &'static str,
    dones: Arc<Mutex<Vec<pi_coding_agent::core::extensions::types::CustomUiDone>>>,
) -> tokio::task::JoinHandle<anyhow::Result<String>> {
    let ui = ui.clone();
    tokio::spawn(async move {
        let factory: pi_coding_agent::core::extensions::types::CustomComponentFactory =
            Arc::new(move |_tui, _theme, _keybindings, done| {
                dones.lock().push(done);
                Box::pin(async move { Ok(Arc::new(Text::new(label, 1, 0, None)) as ComponentRef) })
            });
        custom_typed::<String>(ui.as_ref(), factory, CustomUiOptions::default()).await
    })
}

fn finish(done: &pi_coding_agent::core::extensions::types::CustomUiDone, value: &str) {
    done(Box::new(value.to_string()) as CustomUiResult);
}

// Regression for #9909: a second inline custom component replaced the first one and the first promise never settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stacks_concurrent_inline_custom_components() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions { extensions: vec![capture_ui_extension(slot.clone())], ..Default::default() }).await;
    let ui = fx.capture_ui(&slot).await;

    let dones: Arc<Mutex<Vec<_>>> = Arc::default();
    let first = open_custom(&ui, "custom one", dones.clone());
    fx.wait_for("custom one", |_| fx.viewport().contains("custom one")).await;
    let second = open_custom(&ui, "custom two", dones.clone());
    fx.wait_for("custom two", |_| fx.viewport().contains("custom two")).await;
    assert_eq!(dones.lock().len(), 2);
    assert!(!fx.viewport().contains("custom one"), "{}", fx.viewport());

    let done_two = dones.lock()[1].clone();
    finish(&done_two, "two");
    assert_eq!(resolved(second).await.unwrap(), "two");
    fx.wait_for("custom one again", |_| fx.viewport().contains("custom one")).await;

    let done_one = dones.lock()[0].clone();
    finish(&done_one, "one");
    assert_eq!(resolved(first).await.unwrap(), "one");
    fx.settle().await;
    assert!(!fx.viewport().contains("custom one"), "{}", fx.viewport());
    fx.cleanup();
}

/// A custom form that opens a confirm when it receives input.
struct DiscardForm {
    runtime: tokio::runtime::Handle,
    ui: Arc<dyn ExtensionUIContext>,
    confirmed: Arc<Mutex<Option<tokio::task::JoinHandle<bool>>>>,
    text: Text,
}

impl Component for DiscardForm {
    fn render(&self, width: usize) -> Vec<String> {
        self.text.render(width)
    }

    fn invalidate(&self) {
        self.text.invalidate();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_input(&self, _data: &str) {
        // Input arrives on the terminal's input thread; open the dialog on the test runtime.
        let _guard = self.runtime.enter();
        *self.confirmed.lock() = Some(spawn_confirm(&self.ui, "Discard?", "discard changes", None));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shows_a_confirm_opened_from_an_inline_custom_component_and_then_restores_the_component() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions { extensions: vec![capture_ui_extension(slot.clone())], ..Default::default() }).await;
    let ui = fx.capture_ui(&slot).await;

    let confirmed: Arc<Mutex<Option<tokio::task::JoinHandle<bool>>>> = Arc::default();
    let done: Arc<Mutex<Option<pi_coding_agent::core::extensions::types::CustomUiDone>>> = Arc::default();
    let custom = {
        let ui = ui.clone();
        let confirmed = confirmed.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let form_ui = ui.clone();
            let factory: pi_coding_agent::core::extensions::types::CustomComponentFactory =
                Arc::new(move |_tui, _theme, _keybindings, finish| {
                    *done.lock() = Some(finish);
                    let form = DiscardForm {
                        runtime: tokio::runtime::Handle::current(),
                        ui: form_ui.clone(),
                        confirmed: confirmed.clone(),
                        text: Text::new("custom form", 1, 0, None),
                    };
                    Box::pin(async move { Ok(Arc::new(form) as ComponentRef) })
                });
            custom_typed::<String>(ui.as_ref(), factory, CustomUiOptions::default()).await
        })
    };
    fx.wait_for("the custom form", |_| fx.viewport().contains("custom form")).await;

    fx.terminal.send_input("x");
    fx.wait_for("the confirm", |_| fx.viewport().contains("Discard?")).await;
    fx.terminal.send_input(ESC);
    let confirm = confirmed.lock().take().unwrap();
    assert!(!resolved(confirm).await);

    fx.wait_for("the custom form again", |_| fx.viewport().contains("custom form")).await;
    let finish_form = done.lock().clone().unwrap();
    finish(&finish_form, "kept");
    assert_eq!(resolved(custom).await.unwrap(), "kept");
    fx.settle().await;
    assert!(!fx.viewport().contains("custom form"), "{}", fx.viewport());
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-anthropic-warning.test.ts
// -------------------------------------------------------------------------------------------------

const ANTHROPIC_WARNING: &str = "Anthropic subscription auth is active";

fn anthropic_models() -> Vec<FauxModelDefinition> {
    ["claude-a", "claude-b"]
        .iter()
        .map(|id| FauxModelDefinition { id: id.to_string(), name: Some(id.to_string()), ..Default::default() })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warns_once_when_anthropic_subscription_auth_is_detected() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions {
        provider: Some("anthropic".into()),
        models: anthropic_models(),
        credential: Some(json!({ "type": "api_key", "key": "sk-ant-oat01-test" })),
        settings: Some(json!({ "enabledModels": ["anthropic/claude-a", "anthropic/claude-b"] })),
        ..Default::default()
    })
    .await;
    fx.start().await;
    fx.wait_for("the warning", |s| s.contains(ANTHROPIC_WARNING)).await;
    // Switching models checks again; the warning stays a one-off.
    fx.terminal.send_input("\x10");
    fx.wait_for("the model switch", |s| s.contains("Switched to")).await;
    let screen = fx.settle().await;
    assert_eq!(screen.matches(ANTHROPIC_WARNING).count(), 1, "{screen}");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warns_when_anthropic_oauth_is_stored() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions {
        provider: Some("anthropic".into()),
        models: anthropic_models(),
        credential: Some(json!({ "type": "oauth", "refresh": "r", "access": "a", "expires": 0 })),
        ..Default::default()
    })
    .await;
    fx.start().await;
    fx.wait_for("the warning", |s| s.contains(ANTHROPIC_WARNING)).await;
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_warn_for_non_anthropic_models() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions {
        credential: Some(json!({ "type": "api_key", "key": "sk-ant-oat01-test" })),
        ..Default::default()
    })
    .await;
    fx.start().await;
    let screen = fx.settle().await;
    assert!(!screen.contains(ANTHROPIC_WARNING), "{screen}");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_warn_when_the_anthropic_extra_usage_warning_is_disabled() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions {
        provider: Some("anthropic".into()),
        models: anthropic_models(),
        credential: Some(json!({ "type": "api_key", "key": "sk-ant-oat01-test" })),
        settings: Some(json!({ "warnings": { "anthropicExtraUsage": false } })),
        ..Default::default()
    })
    .await;
    fx.start().await;
    let screen = fx.settle().await;
    assert!(!screen.contains(ANTHROPIC_WARNING), "{screen}");
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-assistant-diagnostics.test.ts
// -------------------------------------------------------------------------------------------------

fn thinking_drop_reply(text: &str) -> FauxResponseStep {
    let message: AssistantMessage = serde_json::from_value(json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "api": "anthropic-messages",
        "provider": "anthropic",
        "model": "claude-fable-5-1",
        "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                   "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
        "stopReason": "stop",
        "timestamp": 1,
        "diagnostics": [{
            "type": "anthropic_input_transformations",
            "timestamp": 1,
            "details": { "transformations": [
                { "type": "thinking_dropped", "path": "messages.2.content.0", "reason": "prefix_binding_mismatch" },
                { "type": "thinking_dropped", "path": "messages.5.content.0", "reason": "prefix_binding_mismatch" },
                { "type": "thinking_dropped", "path": "messages.8.content.0", "reason": "prefix_binding_mismatch" }
            ] }
        }]
    }))
    .unwrap();
    message.into()
}

const DROP_NOTICE: &str = "Anthropic dropped 3 thinking blocks (details in session)";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shows_anthropic_thinking_drops_only_when_cache_miss_notices_are_enabled() {
    let _lock = LOCK.lock().await;
    for enabled in [true, false] {
        let mut fx = Fixture::new(FixtureOptions {
            settings: Some(json!({ "showCacheMissNotices": enabled })),
            ..Default::default()
        })
        .await;
        fx.harness.set_responses(vec![thinking_drop_reply("survived")]);
        fx.start().await;
        fx.type_text("hello").await;
        let screen = fx.wait_for("the reply", |s| s.contains("survived")).await;
        let screen = if enabled { fx.wait_for("the notice", |s| s.contains(DROP_NOTICE)).await } else { screen };
        assert_eq!(screen.contains(DROP_NOTICE), enabled, "{screen}");
        fx.cleanup();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_repeat_unchanged_anthropic_thinking_drops() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions {
        settings: Some(json!({ "showCacheMissNotices": true })),
        ..Default::default()
    })
    .await;
    fx.harness.set_responses(vec![thinking_drop_reply("first reply"), thinking_drop_reply("second reply")]);
    fx.start().await;
    fx.type_text("one").await;
    fx.wait_for("the first notice", |s| s.contains("first reply") && s.contains(DROP_NOTICE)).await;
    fx.type_text("two").await;
    fx.wait_for("the second reply", |s| s.contains("second reply")).await;
    let screen = fx.settle().await;
    assert_eq!(screen.matches(DROP_NOTICE).count(), 1, "{screen}");
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-bug-report-hint.test.ts
// -------------------------------------------------------------------------------------------------

const BUG_HINT: &str = "/bug sends a report to the developers";

fn error_reply(message: &str) -> FauxResponseStep {
    faux_assistant_message(
        Vec::new(),
        FauxMessageOptions {
            stop_reason: Some(pi_ai::types::StopReason::Error),
            error_message: Some(message.to_string()),
            ..Default::default()
        },
    )
    .into()
}

async fn bug_hint_after(reply: FauxResponseStep, error_text: &str) -> String {
    let mut fx = Fixture::new(FixtureOptions::default()).await;
    fx.harness.set_responses(vec![reply]);
    fx.start().await;
    fx.type_text("hello").await;
    fx.wait_for("the error", |s| s.contains(error_text)).await;
    let screen = fx.settle().await;
    fx.cleanup();
    screen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_suggest_reports_for_retryable_provider_failures() {
    let _lock = LOCK.lock().await;
    for failure in [
        "500 Internal Server Error",
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
        "429 Too Many Requests",
        "Provider overloaded",
        "Network connection lost",
        "Request timed out",
    ] {
        let screen = bug_hint_after(error_reply(failure), failure).await;
        assert!(!screen.contains(BUG_HINT), "{failure}:\n{screen}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_suggest_reports_for_cancellations() {
    let _lock = LOCK.lock().await;
    for failure in ["This operation was aborted", "Request cancelled"] {
        let screen = bug_hint_after(error_reply(failure), failure).await;
        assert!(!screen.contains(BUG_HINT), "{failure}:\n{screen}");
    }
    let aborted = faux_assistant_message(
        Vec::new(),
        FauxMessageOptions { stop_reason: Some(pi_ai::types::StopReason::Aborted), ..Default::default() },
    );
    let screen = bug_hint_after(aborted.into(), "Operation aborted").await;
    assert!(!screen.contains(BUG_HINT), "{screen}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suggests_reports_for_unexpected_errors() {
    let _lock = LOCK.lock().await;
    let screen = bug_hint_after(error_reply("Unexpected internal state"), "Unexpected internal state").await;
    assert_eq!(screen.matches(BUG_HINT).count(), 1, "{screen}");
}

// -------------------------------------------------------------------------------------------------
// Shared helpers for the ports below
// -------------------------------------------------------------------------------------------------

fn strip_ansi(text: &str) -> String {
    pi_coding_agent::utils::ansi::strip_ansi(text).unwrap()
}

fn usage(value: Value) -> pi_ai::types::Usage {
    serde_json::from_value(value).unwrap()
}

/// A faux reply that blocks until `release` is notified or the run is aborted.
fn gated_reply(release: Arc<tokio::sync::Notify>, text: &'static str) -> FauxResponseStep {
    FauxResponseStep::Factory(Arc::new(move |_context, options, _state, _model| {
        let release = release.clone();
        let signal = options.signal.clone();
        Box::pin(async move {
            match signal {
                Some(signal) => tokio::select! {
                    _ = release.notified() => {}
                    _ = signal.cancelled() => {}
                },
                None => release.notified().await,
            }
            Ok(pi_ai::providers::faux::faux_text_message(text))
        })
    }))
}

/// An extension that answers `session_before_compact` with a summary once `release` is notified, and
/// reports when compaction started.
fn gated_compactor(
    release: Arc<tokio::sync::Notify>,
    started: Arc<tokio::sync::Notify>,
    summary_usage: Option<pi_ai::types::Usage>,
) -> TestExtension {
    use pi_coding_agent::core::compaction::CompactionResult;
    use pi_coding_agent::core::extensions::types::{SessionBeforeCompactEvent, SessionBeforeCompactResult, on_event};
    let factory: ExtensionFactory = Arc::new(move |pi: ExtensionAPIRef| {
        let release = release.clone();
        let started = started.clone();
        let summary_usage = summary_usage.clone();
        Box::pin(async move {
            on_event::<SessionBeforeCompactEvent, _, _>(&*pi, move |event, _ctx| {
                let release = release.clone();
                let started = started.clone();
                let summary_usage = summary_usage.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(Some(SessionBeforeCompactResult {
                        cancel: None,
                        compaction: Some(CompactionResult {
                            summary: "compacted summary".into(),
                            first_kept_entry_id: event.preparation.first_kept_entry_id.clone(),
                            tokens_before: event.preparation.tokens_before,
                            estimated_tokens_after: None,
                            usage: summary_usage,
                            details: Some(json!({})),
                        }),
                    }))
                }
            })?;
            Ok(())
        })
    });
    TestExtension::new(factory)
}

impl Fixture {
    /// Prompt through the editor and wait for the reply.
    async fn exchange(&self, prompt: &str, reply: &str) {
        self.type_text(prompt).await;
        self.wait_for(reply, |s| s.contains(reply)).await;
        self.wait_idle().await;
    }

    async fn wait_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.harness.session.is_idle() {
            assert!(Instant::now() < deadline, "the session did not become idle");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_until(&self, what: &str, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}:\n{}", self.screen());
            self.mode.tui().request_render(false);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-startup-input.test.ts
// -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restores_a_prompt_submitted_while_managed_tool_setup_is_running() {
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions { skip_init: true, ..Default::default() }).await;
    // Before init finishes, only startup feedback is enabled.
    fx.mode.submit("early prompt");
    assert_eq!(fx.hooks().editor_text(), "early prompt");
    assert!(fx.chat().contains("Startup is still in progress"), "{}", fx.chat());
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queues_a_normal_prompt_submitted_before_the_input_callback_is_installed() {
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions::default()).await;
    // Nothing waits for input yet (the main loop has not started).
    fx.mode.submit(" early prompt ");
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), fx.mode.get_user_input()).await.unwrap().unwrap(), "early prompt");
    // The prompt went into the editor history.
    fx.terminal.send_input("\x1b[A");
    fx.wait_until("the history entry", || fx.hooks().editor_text() == "early prompt").await;
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returns_queued_startup_input_before_installing_a_new_input_callback() {
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions::default()).await;
    fx.mode.submit("queued prompt");
    fx.mode.submit("second prompt");
    let next = || async { tokio::time::timeout(Duration::from_secs(5), fx.mode.get_user_input()).await.unwrap().unwrap() };
    assert_eq!(next().await, "queued prompt");
    assert_eq!(next().await, "second prompt");
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-compaction.test.ts
// -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_the_cache_miss_notice_setting_for_compaction_and_branch_summary_costs() {
    use pi_coding_agent::modes::interactive::interactive_mode::CompactionCostNoticeKind;
    let _lock = LOCK.lock().await;
    let notice_usage = usage(json!({
        "input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40, "totalTokens": 100,
        "cost": { "input": 0.01, "output": 0.02, "cacheRead": 0.03, "cacheWrite": 0.065, "total": 0.125 }
    }));
    let fx = Fixture::new(FixtureOptions { settings: Some(json!({ "showCacheMissNotices": true })), ..Default::default() }).await;
    fx.hooks().add_compaction_cost_notice(CompactionCostNoticeKind::Compaction, notice_usage.clone());
    fx.hooks().add_compaction_cost_notice(CompactionCostNoticeKind::BranchSummary, notice_usage.clone());
    // JS `toFixed` rounds the 0.125 tie up.
    assert!(fx.chat().contains("Compaction: 100 tokens billed (~$0.13)"), "{}", fx.chat());
    assert!(fx.chat().contains("Branch summary: 100 tokens billed (~$0.13)"), "{}", fx.chat());
    fx.cleanup();

    let fx = Fixture::new(FixtureOptions { settings: Some(json!({ "showCacheMissNotices": false })), ..Default::default() }).await;
    let before = fx.hooks().chat_children();
    fx.hooks().add_compaction_cost_notice(CompactionCostNoticeKind::Compaction, notice_usage);
    assert_eq!(fx.hooks().chat_children(), before);
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renders_each_compaction_cost_after_its_summary() {
    use pi_coding_agent::core::session_manager::SessionEntry;
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions { settings: Some(json!({ "showCacheMissNotices": true })), ..Default::default() }).await;
    let entries: Vec<SessionEntry> = serde_json::from_value(json!([
        {
            "type": "compaction", "id": "current", "parentId": "previous", "timestamp": "2025-01-02T00:00:00Z",
            "summary": "current summary", "firstKeptEntryId": "kept", "tokensBefore": 200,
            "usage": { "input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40, "totalTokens": 100,
                       "cost": { "input": 0.01, "output": 0.02, "cacheRead": 0.03, "cacheWrite": 0.04, "total": 0.1 } }
        },
        {
            "type": "compaction", "id": "previous", "parentId": null, "timestamp": "2025-01-01T00:00:00Z",
            "summary": "previous summary", "firstKeptEntryId": "kept", "tokensBefore": 100,
            "usage": { "input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4, "totalTokens": 10,
                       "cost": { "input": 0.001, "output": 0.002, "cacheRead": 0.003, "cacheWrite": 0.004, "total": 0.01 } }
        }
    ]))
    .unwrap();
    fx.hooks().render_session_entries(&entries);
    let chat = fx.chat();
    let position = |needle: &str| chat.find(needle).unwrap_or_else(|| panic!("missing {needle:?}:\n{chat}"));
    let current = position("Compacted from 200 tokens");
    let current_cost = position("Compaction: 100 tokens billed (~$0.10)");
    let previous = position("Compacted from 100 tokens");
    let previous_cost = position("Compaction: 10 tokens billed (~$0.01)");
    assert!(current < current_cost && current_cost < previous && previous < previous_cost, "{chat}");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renders_retained_entries_and_appends_the_latest_summary_cost_at_the_bottom() {
    let _lock = LOCK.lock().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let summary_usage = usage(json!({
        "input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40, "totalTokens": 100,
        "cost": { "input": 0.01, "output": 0.02, "cacheRead": 0.03, "cacheWrite": 0.065, "total": 0.125 }
    }));
    let mut fx = Fixture::new(FixtureOptions {
        settings: Some(json!({ "showCacheMissNotices": true, "compaction": { "keepRecentTokens": 1 } })),
        extensions: vec![gated_compactor(release.clone(), started.clone(), Some(summary_usage))],
        ..Default::default()
    })
    .await;
    fx.harness.set_responses(vec![
        pi_ai::providers::faux::faux_text_message("first reply").into(),
        pi_ai::providers::faux::faux_text_message("second reply").into(),
        pi_ai::providers::faux::faux_text_message("queued reply").into(),
    ]);
    fx.start().await;
    fx.exchange("first question", "first reply").await;
    fx.exchange("second question", "second reply").await;

    fx.type_text("/compact").await;
    tokio::time::timeout(Duration::from_secs(10), started.notified()).await.expect("compaction started");
    // Input during compaction waits for it.
    fx.type_text("after compaction").await;
    fx.wait_for("the queue notice", |s| s.contains("Queued message for after compaction")).await;
    release.notify_one();

    fx.wait_for("the cost notice", |s| s.contains("Compaction: 100 tokens billed (~$0.13)")).await;
    // The flushed queue starts a prompt.
    fx.wait_for("the queued reply", |s| s.contains("queued reply")).await;
    fx.wait_idle().await;
    let chat = fx.chat();
    let summary = chat.find("Compacted from").unwrap_or_else(|| panic!("{chat}"));
    let cost = chat.find("Compaction: 100 tokens billed").unwrap();
    let retained = chat.find("second reply").unwrap();
    let queued = chat.find("after compaction").unwrap();
    assert!(retained < summary && summary < cost && cost < queued, "{chat}");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn updates_the_working_state_when_the_same_agent_run_resumes_after_compaction() {
    use pi_coding_agent::core::agent_session::AgentSessionEvent;
    use pi_coding_agent::modes::interactive::components::status_indicator::StatusIndicatorKind;
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions {
        settings: Some(json!({ "terminal": { "showTerminalProgress": true } })),
        ..Default::default()
    })
    .await;
    let hooks = fx.hooks();
    fx.recording.progress.lock().clear();

    hooks.handle_event(AgentSessionEvent::TurnStart);
    assert_eq!(*fx.recording.progress.lock(), vec![true]);
    assert_eq!(hooks.active_status_indicator_kind(), Some(StatusIndicatorKind::Working));

    hooks.set_working_visible(false);
    hooks.handle_event(AgentSessionEvent::TurnStart);
    assert_eq!(hooks.active_status_indicator_kind(), None);
    fx.cleanup();
}

// Regression test for #9340.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes_interactive_response_aborts_through_agent_session() {
    let _lock = LOCK.lock().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let mut fx = Fixture::new(FixtureOptions::default()).await;
    fx.harness.set_responses(vec![gated_reply(release.clone(), "never shown")]);
    fx.start().await;
    fx.type_text("long task").await;
    fx.wait_until("streaming", || fx.harness.session.is_streaming()).await;

    // Escape while streaming restores the (empty) queue and aborts through AgentSession.
    fx.terminal.send_input(ESC);
    fx.wait_for("the abort", |s| s.contains("Operation aborted")).await;
    fx.wait_idle().await;
    assert!(!fx.screen().contains("never shown"));
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preserves_steering_behavior_when_flushing_into_an_active_agent_run() {
    use pi_coding_agent::modes::interactive::interactive_mode::CompactionQueuedMessageMode;
    let _lock = LOCK.lock().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let mut fx = Fixture::new(FixtureOptions::default()).await;
    fx.harness.set_responses(vec![
        gated_reply(release.clone(), "first answer"),
        pi_ai::providers::faux::faux_text_message("steered answer").into(),
    ]);
    fx.start().await;
    fx.type_text("long task").await;
    fx.wait_until("streaming", || fx.harness.session.is_streaming()).await;

    let hooks = fx.hooks();
    hooks.queue_compaction_message("change direction", CompactionQueuedMessageMode::Steer);
    hooks.flush_compaction_queue(false).await;
    assert!(hooks.compaction_queued_messages().is_empty());
    assert_eq!(fx.harness.session.get_steering_messages(), vec!["change direction".to_string()]);
    fx.wait_for("the steering line", |s| s.contains("Steering: change direction")).await;

    release.notify_one();
    fx.wait_for("the steered answer", |s| s.contains("steered answer")).await;
    assert!(!fx.screen().contains("Error:"), "{}", fx.screen());
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-suspend.test.ts
// -------------------------------------------------------------------------------------------------

#[derive(Default)]
struct FakeSignalState {
    sigint_ignored: AtomicUsize,
    sigint_restored: AtomicUsize,
    sigcont: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    sigcont_cancelled: AtomicUsize,
    sigtstp: AtomicUsize,
    fail: bool,
}

/// Fake process signals (TS spies on `process.on`, `process.once` and `process.kill`).
#[derive(Clone, Default)]
struct FakeSignals(Arc<FakeSignalState>);

impl std::ops::Deref for FakeSignals {
    type Target = FakeSignalState;
    fn deref(&self) -> &FakeSignalState {
        &self.0
    }
}

impl pi_coding_agent::modes::interactive::interactive_mode::SuspendSignals for FakeSignals {
    fn ignore_sigint(&self) -> Box<dyn FnOnce() + Send> {
        self.sigint_ignored.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        Box::new(move || {
            this.sigint_restored.fetch_add(1, Ordering::SeqCst);
        })
    }
    fn once_sigcont(&self, callback: Box<dyn FnOnce() + Send>) -> Box<dyn FnOnce() + Send> {
        *self.sigcont.lock() = Some(callback);
        let this = self.clone();
        Box::new(move || {
            this.sigcont_cancelled.fetch_add(1, Ordering::SeqCst);
            this.sigcont.lock().take();
        })
    }
    fn send_sigtstp(&self) -> std::io::Result<()> {
        self.sigtstp.fetch_add(1, Ordering::SeqCst);
        if self.fail { Err(std::io::Error::other("suspend failed")) } else { Ok(()) }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keeps_sigint_ignored_while_suspended_and_restores_the_tui_on_sigcont() {
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions::default()).await;
    let signals = FakeSignals::default();
    fx.hooks().set_suspend_signals(Arc::new(signals.clone()));
    let (starts, stops) = fx.recording.counts();

    // Ctrl+Z goes through the app.suspend keybinding.
    fx.terminal.send_input("\x1a");
    fx.wait_until("the suspend", || signals.sigtstp.load(Ordering::SeqCst) == 1).await;
    assert_eq!(fx.recording.counts(), (starts, stops + 1));
    assert_eq!(signals.sigint_ignored.load(Ordering::SeqCst), 1);
    assert_eq!(signals.sigint_restored.load(Ordering::SeqCst), 0);
    let sigcont = signals.sigcont.lock().take().expect("a SIGCONT listener");

    let redraws = fx.mode.tui().full_redraws();
    sigcont();
    assert_eq!(signals.sigint_restored.load(Ordering::SeqCst), 1);
    assert_eq!(fx.recording.counts(), (starts + 1, stops + 1));
    // The resumed TUI repaints everything (requestRender(true)).
    fx.wait_until("the full redraw", || fx.mode.tui().full_redraws() > redraws).await;
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleans_up_the_temporary_handlers_if_suspension_fails() {
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions::default()).await;
    let signals = FakeSignals(Arc::new(FakeSignalState { fail: true, ..Default::default() }));
    fx.hooks().set_suspend_signals(Arc::new(signals.clone()));
    let (starts, stops) = fx.recording.counts();

    let error = fx.hooks().suspend().unwrap_err();
    assert_eq!(error.to_string(), "suspend failed");
    assert_eq!(fx.recording.counts(), (starts, stops + 1));
    assert_eq!(signals.sigint_restored.load(Ordering::SeqCst), 1);
    assert_eq!(signals.sigcont_cancelled.load(Ordering::SeqCst), 1);
    assert!(signals.sigcont.lock().is_none());
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-mode-tree-navigation.test.ts
// -------------------------------------------------------------------------------------------------

/// The start of the busy error (the full message wraps on screen).
const TREE_BUSY: &str = "Wait for the current compaction or tree navigation to finish";

impl Fixture {
    /// Open /tree, move from the leaf (the last reply) to the first user message and select it.
    async fn select_first_user_message_in_tree(&self) {
        self.hooks().show_tree_selector();
        self.wait_for("the tree", |_| self.viewport().contains("first")).await;
        self.terminal.send_input("\x1b[A");
        self.settle().await;
        self.terminal.send_input(ENTER);
        self.wait_for("the summary dialog", |_| self.viewport().contains("Summarize branch?")).await;
    }

    /// Start a compaction that waits for `release`.
    async fn start_gated_compaction(&self, started: &tokio::sync::Notify) -> tokio::task::JoinHandle<()> {
        let session = self.harness.session.clone();
        let handle = tokio::spawn(async move {
            let _ = session.compact(None).await;
        });
        tokio::time::timeout(Duration::from_secs(10), started.notified()).await.expect("compaction started");
        assert!(self.harness.session.is_compacting());
        handle
    }
}

// Regression for #9178 / PR #9179: rejection must not replace the active operation's UI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preserves_operation_ui_when_choosing_a_summary_option_while_busy() {
    let _lock = LOCK.lock().await;
    for (choice_keys, choice) in [("\x1b[B", "Summarize"), ("", "No summary")] {
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let mut fx = Fixture::new(FixtureOptions {
            settings: Some(json!({ "compaction": { "keepRecentTokens": 1 } })),
            extensions: vec![gated_compactor(release.clone(), started.clone(), None)],
            ..Default::default()
        })
        .await;
        fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("reply").into()]);
        fx.start().await;
        fx.exchange("first", "reply").await;
        let original_leaf = fx.harness.session_manager.get_leaf_id();

        fx.select_first_user_message_in_tree().await;
        // Compaction starts while the dialog is open.
        let compaction = fx.start_gated_compaction(&started).await;
        let escape_before = fx.hooks().default_editor_on_escape().expect("escape handler");
        let indicator_before = fx.hooks().active_status_indicator_kind();
        if !choice_keys.is_empty() {
            fx.terminal.send_input(choice_keys);
            fx.settle().await;
        }
        fx.terminal.send_input(ENTER);
        fx.wait_for("the busy error", |s| s.contains(TREE_BUSY)).await;

        let hooks = fx.hooks();
        assert_eq!(hooks.active_status_indicator_kind(), indicator_before, "{choice}");
        assert!(Arc::ptr_eq(&hooks.default_editor_on_escape().unwrap(), &escape_before), "{choice}");
        assert_eq!(fx.harness.session_manager.get_leaf_id(), original_leaf, "{choice}");
        assert!(!fx.screen().contains("Navigated to selected point"), "{choice}");

        release.notify_one();
        compaction.await.unwrap();
        fx.cleanup();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allows_navigation_when_compaction_finishes_while_the_dialog_is_open() {
    let _lock = LOCK.lock().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let mut fx = Fixture::new(FixtureOptions {
        settings: Some(json!({ "compaction": { "keepRecentTokens": 1 } })),
        extensions: vec![gated_compactor(release.clone(), started.clone(), None)],
        ..Default::default()
    })
    .await;
    fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("reply").into()]);
    fx.start().await;
    fx.exchange("first", "reply").await;

    let compaction = fx.start_gated_compaction(&started).await;
    fx.select_first_user_message_in_tree().await;
    release.notify_one();
    compaction.await.unwrap();
    assert!(!fx.harness.session.is_compacting());

    fx.terminal.send_input(ENTER); // No summary
    fx.wait_for("the navigation", |s| s.contains("Navigated to selected point")).await;
    assert!(!fx.screen().contains(TREE_BUSY));
    // Navigating to a user message puts its text back into the editor.
    assert_eq!(fx.hooks().editor_text(), "first");
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn still_aborts_an_active_response_before_navigating() {
    let _lock = LOCK.lock().await;
    let release = Arc::new(tokio::sync::Notify::new());
    let mut fx = Fixture::new(FixtureOptions::default()).await;
    fx.harness.set_responses(vec![
        pi_ai::providers::faux::faux_text_message("reply").into(),
        gated_reply(release.clone(), "never shown"),
    ]);
    fx.start().await;
    fx.exchange("first", "reply").await;
    fx.type_text("second").await;
    fx.wait_until("streaming", || fx.harness.session.is_streaming()).await;
    // A steering message is queued; navigating restores it to the editor.
    fx.type_text("queued steer").await;
    fx.wait_for("the steering line", |s| s.contains("Steering: queued steer")).await;

    fx.hooks().show_tree_selector();
    fx.wait_for("the tree", |_| fx.viewport().contains("second")).await;
    // Leaf is the in-flight user message "second"; move up past "reply" to "first".
    fx.terminal.send_input("\x1b[A");
    fx.settle().await;
    fx.terminal.send_input("\x1b[A");
    fx.settle().await;
    fx.terminal.send_input(ENTER);
    fx.wait_for("the summary dialog", |_| fx.viewport().contains("Summarize branch?")).await;
    fx.terminal.send_input(ENTER); // No summary
    fx.wait_for("the navigation", |s| s.contains("Navigated to selected point")).await;
    assert!(!fx.harness.session.is_streaming());
    assert!(!fx.screen().contains("Error:"), "{}", fx.screen());
    assert!(fx.hooks().editor_text().contains("queued steer"), "{}", fx.hooks().editor_text());
    fx.cleanup();
}

// -------------------------------------------------------------------------------------------------
// interactive-tui.test.ts (renderer switching, right-click paste, copy, selection)
// -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaces_the_renderer_and_restores_the_previous_screen_for_resume_hint_exits() {
    use pi_coding_agent::core::settings_schema::FullscreenExitOutput;
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions { tui_mode: Some(TuiMode::Regular), size: Some((60, 20)), ..Default::default() }).await;
    let hooks = fx.hooks();
    let before = fx.mode.tui();
    assert_eq!(before.mode(), pi_tui::tui::TuiMode::Regular);
    let children = before.children().len();
    let focus = before.get_focused_component().expect("focused editor");
    assert_eq!(fx.recording.counts(), (1, 0));

    assert!(hooks.switch_tui_mode(TuiMode::Fullscreen, false));
    let after = fx.mode.tui();
    assert_eq!(after.mode(), pi_tui::tui::TuiMode::Fullscreen);
    assert_eq!(after.children().len(), children);
    assert!(pi_tui::tui::same_component(&after.get_focused_component().unwrap(), &focus));
    assert_eq!(fx.recording.counts(), (2, 1));
    // Components built with the old renderer keep working: typing renders through the new one.
    fx.terminal.send_input("typed after switch");
    fx.wait_until("the typed text", || fx.viewport().contains("typed after switch")).await;

    hooks.stop_interactive_tui(FullscreenExitOutput::ResumeHint);
    assert_eq!(fx.mode.tui().mode(), pi_tui::tui::TuiMode::Fullscreen);
    assert_eq!(fx.recording.counts(), (2, 2));
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feeds_clipboard_text_to_the_focused_component_as_a_bracketed_paste() {
    use pi_coding_agent::utils::clipboard::set_read_clipboard_text_override;
    let _lock = LOCK.lock().await;
    let fx = Fixture::new(FixtureOptions::default()).await;
    set_read_clipboard_text_override(Some(Arc::new(|| Box::pin(async { Ok(Some("clipboard text".to_string())) }))));
    fx.hooks().handle_right_click_paste().await;
    set_read_clipboard_text_override(None);
    assert_eq!(fx.hooks().editor_text(), "clipboard text");
    fx.cleanup();
}

/// Records `copy_to_clipboard` calls.
struct CopySpy(Arc<Mutex<Vec<String>>>);

impl CopySpy {
    fn install() -> CopySpy {
        let copies: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = copies.clone();
        pi_coding_agent::utils::clipboard::set_copy_to_clipboard_override(Some(Arc::new(move |text: String| {
            sink.lock().push(text);
            Box::pin(async { Ok(()) })
        })));
        CopySpy(copies)
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock())
    }
}

impl Drop for CopySpy {
    fn drop(&mut self) {
        pi_coding_agent::utils::clipboard::set_copy_to_clipboard_override(None);
    }
}

impl Fixture {
    /// Drag-select from `needle` (where a screen row's text starts) to four columns into the next row.
    async fn select_two_rows(&self, needle: &str) {
        let lines = self.mode.tui().get_screen_lines();
        let (row, column) = lines
            .iter()
            .enumerate()
            .find_map(|(index, line)| {
                let plain = strip_ansi(line);
                plain.trim_start().starts_with(needle).then(|| (index + 1, plain.len() - plain.trim_start().len() + 1))
            })
            .unwrap_or_else(|| panic!("no row starts with {needle:?}:\n{}", self.viewport()));
        self.terminal.send_input(&format!("\x1b[<0;{column};{row}M"));
        self.terminal.send_input(&format!("\x1b[<32;{};{}M", column + 3, row + 1));
        self.terminal.send_input(&format!("\x1b[<0;{};{}m", column + 3, row + 1));
        self.wait_until("the selection", || self.mode.tui().has_active_selection()).await;
    }

    /// Show `text` as an extension widget above the editor, starting at column 1.
    fn show_widget_lines(&self, ui: &Arc<dyn ExtensionUIContext>, lines: &[&str]) {
        use pi_coding_agent::core::extensions::types::{ExtensionWidgetOptions, WidgetContent};
        ui.set_widget(
            "lines",
            Some(WidgetContent::Lines(lines.iter().map(|l| l.to_string()).collect())),
            ExtensionWidgetOptions::default(),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_an_active_fullscreen_selection_when_copy_on_select_is_disabled() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions {
        tui_mode: Some(TuiMode::Fullscreen),
        settings: Some(json!({ "fullscreenCopyOnSelect": false })),
        extensions: vec![capture_ui_extension(slot.clone())],
        size: Some((60, 20)),
        ..Default::default()
    })
    .await;
    fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("assistant response").into()]);
    let ui = fx.capture_ui(&slot).await;
    fx.exchange("question", "assistant response").await;
    fx.show_widget_lines(&ui, &["alpha", "beta", "gamma"]);
    fx.wait_until("the widget", || fx.viewport().contains("gamma")).await;

    let spy = CopySpy::install();
    fx.select_two_rows("alpha").await;
    assert!(spy.take().is_empty(), "copy-on-select is off");

    fx.hooks().handle_copy_command(true, true).await;
    // Widget lines are padded by one column, so the second row's selection starts with that padding.
    assert_eq!(spy.take(), vec!["alpha\n beta".to_string()]);
    fx.wait_until("the flash", || fx.viewport().contains("Copied!")).await;
    assert!(!fx.screen().contains("Copied last agent message"));
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_the_last_assistant_message_with_an_active_fullscreen_selection_when_copy_on_select_is_enabled() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions {
        tui_mode: Some(TuiMode::Fullscreen),
        extensions: vec![capture_ui_extension(slot.clone())],
        size: Some((60, 20)),
        ..Default::default()
    })
    .await;
    fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("assistant response").into()]);
    let ui = fx.capture_ui(&slot).await;
    fx.exchange("question", "assistant response").await;
    fx.show_widget_lines(&ui, &["alpha", "beta", "gamma"]);
    fx.wait_until("the widget", || fx.viewport().contains("gamma")).await;

    let spy = CopySpy::install();
    fx.select_two_rows("alpha").await;
    // Copy-on-select already copied the selection.
    fx.wait_until("the selection copy", || !spy.0.lock().is_empty()).await;
    spy.take();

    fx.hooks().handle_copy_command(true, true).await;
    assert_eq!(spy.take(), vec!["assistant response".to_string()]);
    fx.wait_until("the flash", || fx.viewport().contains("Copied!")).await;
    assert!(!fx.screen().contains("Copied last agent message"));
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flashes_copied_for_the_copy_shortcut_in_fullscreen_mode() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions { tui_mode: Some(TuiMode::Fullscreen), size: Some((60, 20)), ..Default::default() }).await;
    fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("assistant response").into()]);
    fx.start().await;
    fx.exchange("question", "assistant response").await;
    let spy = CopySpy::install();
    fx.hooks().handle_copy_command(true, true).await;
    assert_eq!(spy.take(), vec!["assistant response".to_string()]);
    fx.wait_until("the flash", || fx.viewport().contains("Copied!")).await;
    assert!(!fx.screen().contains("Copied last agent message") && !fx.screen().contains("Error:"));
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keeps_the_status_line_confirmation_for_the_copy_shortcut_in_regular_mode() {
    let _lock = LOCK.lock().await;
    let mut fx = Fixture::new(FixtureOptions { tui_mode: Some(TuiMode::Regular), ..Default::default() }).await;
    fx.harness.set_responses(vec![pi_ai::providers::faux::faux_text_message("assistant response").into()]);
    fx.start().await;
    fx.exchange("question", "assistant response").await;
    let spy = CopySpy::install();
    fx.hooks().handle_copy_command(true, true).await;
    assert_eq!(spy.take(), vec!["assistant response".to_string()]);
    fx.wait_for("the status", |s| s.contains("Copied last agent message to clipboard")).await;
    fx.cleanup();
}

// Regression test for #9311: selection coordinates survived session switches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drops_the_fullscreen_selection_when_session_entries_are_re_rendered() {
    let _lock = LOCK.lock().await;
    let slot = UiSlot::default();
    let mut fx = Fixture::new(FixtureOptions {
        tui_mode: Some(TuiMode::Fullscreen),
        settings: Some(json!({ "fullscreenCopyOnSelect": false })),
        extensions: vec![capture_ui_extension(slot.clone())],
        size: Some((60, 20)),
        ..Default::default()
    })
    .await;
    let ui = fx.capture_ui(&slot).await;
    fx.show_widget_lines(&ui, &["alpha", "beta", "gamma"]);
    fx.wait_until("the widget", || fx.viewport().contains("gamma")).await;
    fx.select_two_rows("alpha").await;
    assert!(fx.mode.tui().has_active_selection());

    fx.hooks().render_session_entries(&[]);
    assert!(!fx.mode.tui().has_active_selection());
    fx.cleanup();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn markdown_transformers_receive_the_rendered_session_entry_identity() {
    use pi_coding_agent::core::session_manager::SessionEntry;
    let _lock = LOCK.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let factory: ExtensionFactory = Arc::new(move |pi| {
        let captured = captured.clone();
        Box::pin(async move {
            pi.register_markdown_transformer(Arc::new(move |markdown, context| {
                captured.lock().push((markdown.to_string(), serde_json::to_value(context).unwrap()));
                markdown.to_string()
            }))?;
            Ok(())
        })
    });
    let fx = Fixture::new(FixtureOptions {
        extensions: vec![TestExtension::new(factory)],
        settings: Some(json!({"hideThinkingBlock": false})),
        ..Default::default()
    }).await;
    let timestamp = "2025-01-15T10:30:00.000Z";
    let assistant = json!({
        "role": "assistant", "content": [
            {"type": "thinking", "thinking": "reasoning"},
            {"type": "text", "text": "answer"}
        ],
        "api": "openai-responses", "provider": "test", "model": "test",
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                  "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "stop", "timestamp": 123
    });
    let entries: Vec<SessionEntry> = serde_json::from_value(json!([
        {"type": "message", "id": "user-entry", "parentId": null, "timestamp": timestamp,
         "message": {"role": "user", "content": "restored", "timestamp": 123}},
        {"type": "message", "id": "skill-entry", "parentId": "user-entry", "timestamp": timestamp,
         "message": {"role": "user", "content": "<skill name=\"test\" location=\"/tmp/test/SKILL.md\">\ninstructions\n</skill>\n\nskill user text", "timestamp": 123}},
        {"type": "message", "id": "assistant-one", "parentId": "skill-entry", "timestamp": timestamp,
         "message": assistant},
        {"type": "message", "id": "assistant-two", "parentId": "assistant-one", "timestamp": "2025-01-16T00:00:00Z",
         "message": assistant}
    ])).unwrap();
    seen.lock().clear();
    fx.hooks().render_session_entries(&entries);
    fx.chat();
    let contexts = seen.lock().clone();
    for (id, kind, text, stamp) in [
        ("user-entry", "user", "restored", timestamp),
        ("skill-entry", "user", "skill user text", timestamp),
        ("assistant-one", "assistant-thinking", "reasoning", timestamp),
        ("assistant-one", "assistant", "answer", timestamp),
        ("assistant-two", "assistant-thinking", "reasoning", "2025-01-16T00:00:00Z"),
        ("assistant-two", "assistant", "answer", "2025-01-16T00:00:00Z"),
    ] {
        assert!(contexts.iter().any(|(markdown, ctx)| markdown == text
            && ctx["messageType"] == kind && ctx["messageId"] == id
            && ctx["timestamp"] == stamp && ctx["isStreaming"] == false), "{contexts:?}");
    }
    fx.cleanup();
}
