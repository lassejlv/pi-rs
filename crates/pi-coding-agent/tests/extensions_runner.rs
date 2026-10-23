//! Port of `extensions-runner.test.ts`. Extensions written as files in the TS tests are loaded
//! as JS through the pi-jsrt bridge; inline TS factories become Rust factories.

mod extensions_support;

use std::sync::{Arc, OnceLock, Weak};

use extensions_support::*;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_coding_agent::core::event_bus::{EventBus, create_event_bus};
use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, load_extension_from_factory, load_extensions,
};
use pi_coding_agent::core::extensions::runner::{
    BoundaryBaseEvent, BoundaryContextBuilder, ExtensionRunner, NoOpUIContext,
    emit_project_trust_event,
};
use pi_coding_agent::core::extensions::types::*;
use pi_coding_agent::core::keybindings::KeybindingsManager;
use pi_tui::keybindings::{KeySpec, KeybindingsConfig};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

fn factory<F>(f: F) -> ExtensionFactory
where
    F: Fn(ExtensionAPIRef) -> anyhow::Result<()> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |pi| {
        let f = f.clone();
        Box::pin(async move { f(pi) })
    })
}

fn handler<F>(f: F) -> ExtensionHandler
where
    F: Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |_event, _ctx| {
        let f = f.clone();
        Box::pin(async move {
            f().await;
            Ok(None)
        })
    })
}

fn push(calls: &Arc<Mutex<Vec<&'static str>>>, name: &'static str) -> ExtensionHandler {
    let calls = calls.clone();
    handler(move || {
        calls.lock().push(name);
        Box::pin(async {})
    })
}

fn agent_end() -> ExtensionEvent {
    ExtensionEvent::AgentEnd(AgentEndEvent {
        messages: Vec::new(),
    })
}

fn bus() -> Arc<dyn EventBus> {
    create_event_bus()
}

async fn runner_for(
    dir: &ExtDir,
    extensions: &[ExtensionRef],
    runtime: Arc<ExtensionRuntime>,
) -> Arc<ExtensionRunner> {
    let root = dir.root();
    ExtensionRunner::new(
        extensions,
        runtime,
        &root,
        session_manager(&root),
        model_registry().await,
    )
}

fn bind(runner: &ExtensionRunner) {
    runner.bind_core(
        Arc::new(RecordingActions::default()),
        Arc::new(TestContextActions::default()),
        None,
    );
}

fn default_keybindings() -> KeybindingsConfig {
    KeybindingsManager::new(None, None).get_effective_config()
}

fn with_binding(mut config: KeybindingsConfig, id: &str, keys: KeySpec) -> KeybindingsConfig {
    match config.iter_mut().find(|(k, _)| k == id) {
        Some(entry) => entry.1 = Some(keys),
        None => config.push((id.to_string(), Some(keys))),
    }
    config
}

fn first_key(config: &KeybindingsConfig, id: &str) -> String {
    match config
        .iter()
        .find(|(k, _)| k == id)
        .and_then(|(_, v)| v.clone())
    {
        Some(KeySpec::One(k)) => k,
        Some(KeySpec::Many(keys)) => keys.first().cloned().unwrap_or_default(),
        None => String::new(),
    }
}

fn shortcut_code(key: &str, description: &str) -> String {
    format!(
        r#"export default function(pi) {{
	pi.registerShortcut("{key}", {{ description: "{description}", handler: async () => {{}} }});
}}"#
    )
}

fn warned(runner: &ExtensionRunner, needle: &str) -> bool {
    runner
        .get_shortcut_diagnostics()
        .iter()
        .any(|d| d.message.contains(needle))
}

async fn shortcut_runner(dir: &ExtDir) -> Arc<ExtensionRunner> {
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    dir.runner(&result).await
}

fn provider_model_config() -> ProviderConfig {
    let model: ProviderChatModelConfig = serde_json::from_value(json!({
        "id": "instant-model",
        "name": "Instant Model",
        "reasoning": false,
        "input": ["text"],
        "cost": {
            "input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 1.25,
            "tiers": [{ "inputTokensAbove": 272000, "input": 2, "output": 3, "cacheRead": 0.2, "cacheWrite": 2.5 }]
        },
        "contextWindow": 128000,
        "maxTokens": 4096
    }))
    .unwrap();
    ProviderConfig {
        base_url: Some("https://provider.test/v1".into()),
        api_key: Some("provider-test-key".into()),
        api: Some(serde_json::from_value(json!("openai-completions")).unwrap()),
        models: Some(vec![ProviderModelConfig::Chat(model)]),
        ..Default::default()
    }
}

// ----------------------------------------------------------------------------
// scopedModels / project_trust
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn scoped_models_reflect_the_context_action() {
    let dir = ExtDir::new();
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    assert!(runner.create_context().scoped_models().unwrap().is_empty());

    let model = model_registry().await.get_all().into_iter().next();
    let scoped: Vec<pi_coding_agent::core::model_resolver::ScopedModel> = model
        .map(|model| {
            vec![pi_coding_agent::core::model_resolver::ScopedModel {
                model,
                thinking_level: None,
            }]
        })
        .unwrap_or_default();
    runner.bind_core(
        Arc::new(RecordingActions::default()),
        Arc::new(TestContextActions {
            scoped_models: scoped.clone(),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(
        runner.create_context().scoped_models().unwrap().len(),
        scoped.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn project_trust_continues_past_undecided_handlers() {
    let dir = ExtDir::new();
    let undecided = dir.write(
        "undecided.ts",
        r#"export default function(pi) { pi.on("project_trust", () => ({ trusted: "undecided", remember: true })); }"#,
    );
    let decided = dir.write(
        "decided.ts",
        r#"export default function(pi) { pi.on("project_trust", () => ({ trusted: "no", remember: true })); }"#,
    );
    let result = load_extensions(&[undecided, decided], &dir.root(), None, None)
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let outcome = emit_project_trust_event(
        &result,
        &ProjectTrustEvent { cwd: dir.root() },
        &ProjectTrustContext {
            cwd: dir.root(),
            mode: ExtensionMode::Tui,
            has_ui: false,
            ui: Arc::new(NoOpUIContext),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(&outcome.result).unwrap(),
        json!({ "trusted": "no", "remember": true })
    );
    assert!(outcome.errors.is_empty());
}

// ----------------------------------------------------------------------------
// Shortcut conflicts
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_conflicting_with_reserved_builtin_is_skipped() {
    let dir = ExtDir::new();
    dir.write(
        "conflict.ts",
        &shortcut_code("ctrl+c", "Conflicts with built-in"),
    );
    let runner = shortcut_runner(&dir).await;
    let shortcuts = runner.get_shortcuts(&default_keybindings());
    assert!(warned(&runner, "conflicts with built-in"));
    assert!(!shortcuts.contains_key("ctrl+c"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_allowed_when_reserved_default_was_rebound() {
    let dir = ExtDir::new();
    dir.write(
        "rebinding.ts",
        &shortcut_code("ctrl+p", "Uses freed default"),
    );
    let runner = shortcut_runner(&dir).await;
    let keybindings = with_binding(
        default_keybindings(),
        "app.model.cycleForward",
        KeySpec::One("ctrl+n".into()),
    );
    let shortcuts = runner.get_shortcuts(&keybindings);
    assert!(shortcuts.contains_key("ctrl+p"));
    assert!(!warned(&runner, "conflicts with built-in"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_overriding_non_reserved_builtin_warns_but_is_allowed() {
    let dir = ExtDir::new();
    let key = first_key(&default_keybindings(), "app.clipboard.pasteImage");
    dir.write(
        "non-reserved.ts",
        &shortcut_code(&key, "Overrides non-reserved"),
    );
    let runner = shortcut_runner(&dir).await;
    let shortcuts = runner.get_shortcuts(&default_keybindings());
    assert!(warned(
        &runner,
        "built-in shortcut for app.clipboard.pasteImage"
    ));
    assert!(shortcuts.contains_key(&key));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_blocked_for_rebound_reserved_action() {
    let dir = ExtDir::new();
    dir.write(
        "rebound-reserved.ts",
        &shortcut_code("ctrl+x", "Conflicts with rebound reserved"),
    );
    let runner = shortcut_runner(&dir).await;
    let keybindings = with_binding(
        default_keybindings(),
        "app.interrupt",
        KeySpec::One("ctrl+x".into()),
    );
    let shortcuts = runner.get_shortcuts(&keybindings);
    assert!(warned(&runner, "conflicts with built-in"));
    assert!(!shortcuts.contains_key("ctrl+x"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_blocked_when_reserved_key_is_shared_with_non_reserved_actions() {
    let dir = ExtDir::new();
    dir.write(
        "shared-reserved.ts",
        &shortcut_code("ctrl+p", "Conflicts with shared reserved default"),
    );
    let runner = shortcut_runner(&dir).await;
    let shortcuts = runner.get_shortcuts(&default_keybindings());
    assert!(warned(&runner, "conflicts with built-in"));
    assert!(!shortcuts.contains_key("ctrl+p"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_blocked_when_reserved_action_has_multiple_keys() {
    let dir = ExtDir::new();
    dir.write(
        "multi-reserved.ts",
        &shortcut_code("ctrl+y", "Conflicts with multi-key reserved"),
    );
    let runner = shortcut_runner(&dir).await;
    let keybindings = with_binding(
        default_keybindings(),
        "app.clear",
        KeySpec::Many(vec!["ctrl+x".into(), "ctrl+y".into()]),
    );
    let shortcuts = runner.get_shortcuts(&keybindings);
    assert!(warned(&runner, "conflicts with built-in"));
    assert!(!shortcuts.contains_key("ctrl+y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_allowed_when_non_reserved_action_has_multiple_keys() {
    let dir = ExtDir::new();
    dir.write(
        "multi-non-reserved.ts",
        &shortcut_code("ctrl+y", "Overrides multi-key non-reserved"),
    );
    let runner = shortcut_runner(&dir).await;
    let keybindings = with_binding(
        default_keybindings(),
        "app.clipboard.pasteImage",
        KeySpec::Many(vec!["ctrl+x".into(), "ctrl+y".into()]),
    );
    let shortcuts = runner.get_shortcuts(&keybindings);
    assert!(warned(
        &runner,
        "built-in shortcut for app.clipboard.pasteImage"
    ));
    assert!(shortcuts.contains_key("ctrl+y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcut_registered_by_two_extensions_warns_and_last_wins() {
    let dir = ExtDir::new();
    dir.write("ext1.ts", &shortcut_code("ctrl+shift+x", "First extension"));
    dir.write(
        "ext2.ts",
        &shortcut_code("ctrl+shift+x", "Second extension"),
    );
    let runner = shortcut_runner(&dir).await;
    let shortcuts = runner.get_shortcuts(&default_keybindings());
    assert!(warned(&runner, "shortcut conflict"));
    assert!(shortcuts.contains_key("ctrl+shift+x"));
}

// ----------------------------------------------------------------------------
// Tool and command collection
// ----------------------------------------------------------------------------

fn tool_code(name: &str, description: &str) -> String {
    format!(
        r#"import {{ Type }} from "typebox";
export default function(pi) {{
	pi.registerTool({{
		name: "{name}",
		label: "{name}",
		description: "{description}",
		parameters: Type.Object({{}}),
		execute: async () => ({{ content: [{{ type: "text", text: "ok" }}], details: {{}} }}),
	}});
}}"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn collects_tools_from_multiple_extensions() {
    let dir = ExtDir::new();
    dir.write("tool-a.ts", &tool_code("tool_a", "Test tool"));
    dir.write("tool-b.ts", &tool_code("tool_b", "Test tool"));
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let mut names: Vec<String> = runner
        .get_all_registered_tools()
        .iter()
        .map(|t| t.definition.name.clone())
        .collect();
    names.sort();
    assert_eq!(names, ["tool_a", "tool_b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_first_tool_with_duplicate_name() {
    let dir = ExtDir::new();
    dir.write("a-first.ts", &tool_code("shared", "first"));
    dir.write("b-second.ts", &tool_code("shared", "second"));
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let tools = runner.get_all_registered_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].definition.description, "first");
}

fn command_code(name: &str, description: &str) -> String {
    format!(
        r#"export default function(pi) {{
	pi.registerCommand("{name}", {{ description: "{description}", handler: async () => {{}} }});
}}"#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn collects_commands_and_resolves_invocation_names() {
    let dir = ExtDir::new();
    dir.write("cmd-a.ts", &command_code("cmd-a", "Test command"));
    dir.write("cmd-b.ts", &command_code("cmd-b", "Test command"));
    dir.write("cmd.ts", &command_code("my-cmd", "My command"));
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let mut names: Vec<String> = runner
        .get_registered_commands()
        .iter()
        .map(|c| c.invocation_name.clone())
        .collect();
    names.sort();
    assert_eq!(names, ["cmd-a", "cmd-b", "my-cmd"]);
    let cmd = runner.get_command("my-cmd").expect("my-cmd");
    assert_eq!(cmd.name, "my-cmd");
    assert_eq!(cmd.description.as_deref(), Some("My command"));
    assert!(runner.get_command("not-exists").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn suffixes_duplicate_commands_in_insertion_order() {
    let dir = ExtDir::new();
    dir.write("cmd-a.ts", &command_code("shared-cmd", "First command"));
    dir.write("cmd-b.ts", &command_code("shared-cmd", "Second command"));
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let commands = runner.get_registered_commands();
    assert_eq!(
        commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["shared-cmd", "shared-cmd"]
    );
    assert_eq!(
        commands
            .iter()
            .map(|c| c.invocation_name.as_str())
            .collect::<Vec<_>>(),
        ["shared-cmd:1", "shared-cmd:2"]
    );
    assert!(runner.get_command_diagnostics().is_empty());
    assert_eq!(
        runner
            .get_command("shared-cmd:1")
            .unwrap()
            .description
            .as_deref(),
        Some("First command")
    );
    assert_eq!(
        runner
            .get_command("shared-cmd:2")
            .unwrap()
            .description
            .as_deref(),
        Some("Second command")
    );
}

// ----------------------------------------------------------------------------
// Context creation
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn context_exposes_signal_mode_ui_and_trust() {
    let dir = ExtDir::new();
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;

    let token = CancellationToken::new();
    let actions = Arc::new(TestContextActions {
        project_trusted: false,
        ..Default::default()
    });
    *actions.signal.lock() = Some(token.clone());
    runner.bind_core(Arc::new(RecordingActions::default()), actions, None);

    let ctx = runner.create_context();
    let signal = ctx.signal().unwrap().expect("signal");
    assert!(!signal.is_cancelled());
    token.cancel();
    assert!(signal.is_cancelled());
    assert!(!ctx.is_project_trusted().unwrap());

    assert_eq!(ctx.mode().unwrap(), ExtensionMode::Print);
    assert!(!ctx.has_ui().unwrap());

    runner.set_ui_context(Some(Arc::new(NoOpUIContext)), Some(ExtensionMode::Rpc));
    let ctx = runner.create_context();
    assert_eq!(ctx.mode().unwrap(), ExtensionMode::Rpc);
    assert!(ctx.has_ui().unwrap());

    runner.set_ui_context(Some(Arc::new(NoOpUIContext)), Some(ExtensionMode::Tui));
    assert_eq!(runner.create_context().mode().unwrap(), ExtensionMode::Tui);
}

// ----------------------------------------------------------------------------
// Error handling
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn error_listeners_receive_handler_errors() {
    let dir = ExtDir::new();
    dir.write("throws.ts", r#"export default function(pi) { pi.on("context", async () => { throw new Error("Handler error!"); }); }"#);
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let errors = collect_errors(&runner);
    runner.emit_context(&[]).await.unwrap();
    let errors = errors.lock();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].error.contains("Handler error!"));
    assert_eq!(errors[0].event, "context");
}

fn user_bash(dir: &ExtDir, command: &str) -> UserBashEvent {
    UserBashEvent {
        command: command.into(),
        exclude_from_context: false,
        cwd: dir.root(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn user_bash_fails_closed_when_handler_throws() {
    let dir = ExtDir::new();
    dir.write("throws.ts", r#"export default function(pi) { pi.on("user_bash", async () => { throw new Error("Routing failed"); }); }"#);
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let errors = collect_errors(&runner);
    let err = runner
        .emit_user_bash(&user_bash(&dir, "pwd"))
        .await
        .err()
        .expect("rejects");
    assert!(err.to_string().contains("Routing failed"), "{err}");
    assert_eq!(errors.lock()[0].event, "user_bash");
    assert_eq!(errors.lock()[0].error, "Routing failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn user_bash_fails_closed_on_invalid_results() {
    for handler_result in [
        "{}",
        "{ operations: null }",
        "{ operations: {} }",
        "{ result: null }",
        r#"{ result: { output: "handled" } }"#,
        r#"{ operations: { exec: async () => ({ exitCode: 0 }) }, result: { output: "handled", exitCode: 0, cancelled: false, truncated: false } }"#,
    ] {
        let dir = ExtDir::new();
        dir.write(
            "invalid-result.ts",
            &format!(r#"export default function(pi) {{ pi.on("user_bash", async () => ({handler_result})); }}"#),
        );
        let result = dir.discover().await;
        let runner = dir.runner(&result).await;
        let errors = collect_errors(&runner);
        let err = runner
            .emit_user_bash(&user_bash(&dir, "pwd"))
            .await
            .err()
            .unwrap_or_else(|| panic!("{handler_result}"));
        assert!(
            err.to_string().contains("Invalid user_bash handler result"),
            "{handler_result}: {err}"
        );
        assert_eq!(errors.lock()[0].event, "user_bash");
        assert!(
            errors.lock()[0]
                .error
                .contains("Invalid user_bash handler result")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn user_bash_accepts_valid_operations_and_results() {
    let dir = ExtDir::new();
    dir.write(
        "valid-results.ts",
        r#"export default function(pi) {
	pi.on("user_bash", async (event) => {
		if (event.command === "operations") return { operations: { exec: async () => ({ exitCode: 0 }) } };
		return { result: { output: "handled", exitCode: 0, cancelled: false, truncated: false } };
	});
}"#,
    );
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let ops = runner
        .emit_user_bash(&user_bash(&dir, "operations"))
        .await
        .unwrap();
    let Some(UserBashEventResult::Operations(ops)) = ops else {
        panic!("expected operations")
    };
    let exec = (ops.exec)(
        "echo hi".into(),
        dir.root(),
        pi_coding_agent::core::tools::bash::BashOperationsOptions {
            on_data: Arc::new(|_| {}),
            signal: None,
            timeout: None,
            env: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(exec.exit_code, Some(0));

    let res = runner
        .emit_user_bash(&user_bash(&dir, "result"))
        .await
        .unwrap();
    let Some(UserBashEventResult::Result(res)) = res else {
        panic!("expected result")
    };
    assert_eq!(res.output, "handled");
    assert_eq!(res.exit_code, Some(0));
    assert!(!res.cancelled && !res.truncated);
}

// ----------------------------------------------------------------------------
// Renderers and flags
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn gets_markdown_transformers_and_renderers() {
    let dir = ExtDir::new();
    let transformer = r#"export default function(pi) { pi.registerMarkdownTransformer((markdown) => markdown); }"#;
    dir.write("markdown-renderer-a.ts", transformer);
    dir.write("markdown-renderer-b.ts", transformer);
    dir.write(
        "renderer.ts",
        r#"export default function(pi) { pi.registerMessageRenderer("my-type", () => null); }"#,
    );
    dir.write(
        "entry-renderer.ts",
        r#"export default function(pi) { pi.registerEntryRenderer("my-entry", () => null); }"#,
    );
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let runner = dir.runner(&result).await;
    assert_eq!(runner.get_markdown_transformers().len(), 2);
    assert!(runner.get_message_renderer("my-type").is_some());
    assert!(runner.get_message_renderer("not-exists").is_none());
    assert!(runner.get_entry_renderer("my-entry").is_some());
    assert!(runner.get_entry_renderer("not-exists").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn collects_flags_and_keeps_the_first_duplicate() {
    let dir = ExtDir::new();
    dir.write("with-flag.ts", r#"export default function(pi) { pi.registerFlag("my-flag", { description: "My flag" }); }"#);
    dir.write(
        "a-first.ts",
        r#"export default function(pi) { pi.registerFlag("shared-flag", { description: "first", type: "boolean", default: true }); }"#,
    );
    dir.write(
        "b-second.ts",
        r#"export default function(pi) { pi.registerFlag("shared-flag", { description: "second", type: "boolean", default: false }); }"#,
    );
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let runner = dir.runner(&result).await;
    let flags = runner.get_flags();
    assert!(flags.contains_key("my-flag"));
    assert_eq!(flags["shared-flag"].description.as_deref(), Some("first"));
    assert_eq!(
        result.runtime.flag_values.lock().get("shared-flag"),
        Some(&FlagValue::Bool(true))
    );

    runner.set_flag_value("--test-flag", FlagValue::Bool(true));
    assert_eq!(
        result.runtime.flag_values.lock().get("--test-flag"),
        Some(&FlagValue::Bool(true))
    );
}

// ----------------------------------------------------------------------------
// before_agent_start
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn before_agent_start_keeps_get_system_prompt_in_sync() {
    let dir = ExtDir::new();
    for (name, suffix) in [
        ("before-agent-start-1.ts", "first"),
        ("before-agent-start-2.ts", "second"),
    ] {
        dir.write(
            name,
            &format!(
                r#"export default function(pi) {{
	pi.on("before_agent_start", async (_event, ctx) => ({{ systemPrompt: ctx.getSystemPrompt() + "\n{suffix}" }}));
}}"#
            ),
        );
    }
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.extensions.len(), 2);
    let runner = dir.runner(&result).await;
    let errors = collect_errors(&runner);
    bind(&runner);

    let options: BuildSystemPromptOptions =
        serde_json::from_value(json!({ "cwd": dir.root(), "customPrompt": "base" })).unwrap();
    let chained = runner
        .emit_before_agent_start(
            &BeforeAgentStartKind::User {
                prompt: "hello".into(),
                images: None,
            },
            &options,
        )
        .await
        .unwrap();
    assert!(errors.lock().is_empty(), "{:?}", errors.lock());
    assert!(chained.messages.is_empty());
    let prompt = chained
        .system_prompt_options
        .force_system_prompt
        .clone()
        .expect("prompt");
    assert!(prompt.starts_with("base"), "{prompt}");
    assert!(prompt.ends_with("\nfirst\nsecond"), "{prompt}");
}

// ----------------------------------------------------------------------------
// Tool renderers (Rust factories)
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn resolves_tool_renderers_in_load_order() {
    let dir = ExtDir::new();
    let runtime = create_extension_runtime();
    let render_call: ToolRenderCallFn =
        Arc::new(|_, _, _| Arc::new(pi_tui::components::text::Text::new("", 0, 0, None)));
    let rc = render_call.clone();
    let first = load_extension_from_factory(
        &factory(move |pi| {
            let rc = rc.clone();
            pi.register_tool_renderer(Arc::new(move |tool_name, next| {
                if tool_name == "a" {
                    Some(ToolRenderers {
                        render_shell: None,
                        render_call: Some(rc.clone()),
                        render_result: None,
                    })
                } else {
                    next()
                }
            }))
        }),
        &dir.root(),
        bus(),
        runtime.clone(),
        None,
    )
    .await
    .unwrap();
    let second = load_extension_from_factory(
        &factory(|pi| {
            pi.register_tool_renderer(Arc::new(|_tool_name, next| {
                next().or(Some(ToolRenderers {
                    render_shell: Some(RenderShell::SelfRendered),
                    render_call: None,
                    render_result: None,
                }))
            }))
        }),
        &dir.root(),
        bus(),
        runtime.clone(),
        None,
    )
    .await
    .unwrap();
    let runner = runner_for(&dir, &[first, second], runtime).await;

    let a = runner
        .resolve_tool_renderers("a", Arc::new(|| None))
        .unwrap();
    assert!(Arc::ptr_eq(a.render_call.as_ref().unwrap(), &render_call));
    let b = runner
        .resolve_tool_renderers("b", Arc::new(|| None))
        .unwrap();
    assert_eq!(b.render_shell, Some(RenderShell::SelfRendered));
    assert!(b.render_call.is_none());
    let base_call = render_call.clone();
    let b = runner
        .resolve_tool_renderers(
            "b",
            Arc::new(move || {
                Some(ToolRenderers {
                    render_shell: None,
                    render_call: Some(base_call.clone()),
                    render_result: None,
                })
            }),
        )
        .unwrap();
    assert!(b.render_shell.is_none());
    assert!(Arc::ptr_eq(b.render_call.as_ref().unwrap(), &render_call));
}

// ----------------------------------------------------------------------------
// Boundary chaining (JS: handlers mutate the shared draft list)
// ----------------------------------------------------------------------------

fn preview_builder(fail_on_context_edit: bool) -> BoundaryContextBuilder {
    Arc::new(move |entries: Vec<SessionBoundaryDraft>| {
        Box::pin(async move {
            let entries: Vec<Value> = entries
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect();
            if fail_on_context_edit && entries.iter().any(|e| e["type"] == "context_edit") {
                anyhow::bail!("Entry missing not found");
            }
            let context_entries: Vec<Value> = if fail_on_context_edit {
                Vec::new()
            } else {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        json!({
                            "sourceEntry": {
                                "type": "custom",
                                "id": format!("draft-{index}"),
                                "parentId": null,
                                "timestamp": "",
                                "customType": entry["type"],
                            },
                            "messages": [],
                        })
                    })
                    .collect()
            };
            Ok(serde_json::from_value(json!({
                "contextEntries": context_entries,
                "contextMessages": [],
                "llmMessages": [],
                "pendingMessages": [],
                "canContinue": false,
            }))?)
        })
    })
}

async fn observing_runner(
    dir: &ExtDir,
    files: &[(&str, &str)],
) -> (Arc<ExtensionRunner>, Arc<Mutex<Vec<Value>>>) {
    let mut paths = Vec::new();
    for (name, code) in files {
        paths.push(dir.write(name, code));
    }
    let bus = bus();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    bus.on("observe", Arc::new(move |v| sink.lock().push(v)));
    let result = load_extensions(&paths, &dir.root(), Some(bus), None)
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    (dir.runner(&result).await, observed)
}

fn settle() -> BoundaryBaseEvent {
    BoundaryBaseEvent::AgentBeforeSettle {
        outcome: AgentActivityOutcome::Completed,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn boundary_chains_shared_drafts_and_preserves_omitted_fields() {
    let dir = ExtDir::new();
    let (runner, observed) = observing_runner(
        &dir,
        &[
            (
                "a-first.js",
                r#"export default function(pi) {
	pi.on("agent_before_settle", (event) => {
		pi.events.emit("observe", { entries: event.entries.length, continuation: event.continue, preview: event.context.contextEntries.length });
		event.entries.push({ type: "custom", customType: "first", data: 1 });
		return { continue: true };
	});
}"#,
            ),
            (
                "b-second.js",
                r#"export default function(pi) {
	pi.on("agent_before_settle", (event) => {
		pi.events.emit("observe", { entries: event.entries.length, continuation: event.continue, preview: event.context.contextEntries.length });
		return { entries: [] };
	});
}"#,
            ),
        ],
    )
    .await;
    let result = runner
        .emit_boundary(&settle(), preview_builder(false))
        .await
        .unwrap();
    assert_eq!(
        *observed.lock(),
        vec![
            json!({ "entries": 0, "continuation": false, "preview": 0 }),
            json!({ "entries": 1, "continuation": true, "preview": 1 }),
        ]
    );
    assert!(result.entries.is_empty());
    assert!(result.r#continue);
}

#[tokio::test(flavor = "multi_thread")]
async fn boundary_reports_invalid_previews_and_lets_later_handlers_repair() {
    let dir = ExtDir::new();
    let (runner, observed) = observing_runner(
        &dir,
        &[
            (
                "a-invalid.js",
                r#"export default function(pi) {
	pi.on("agent_before_settle", () => ({ entries: [{ type: "context_edit", targetId: "missing", replacement: null }] }));
}"#,
            ),
            (
                "b-repair.js",
                r#"export default function(pi) {
	pi.on("agent_before_settle", (event) => {
		pi.events.emit("observe", event.entries.length);
		return { entries: [] };
	});
}"#,
            ),
        ],
    )
    .await;
    let errors = collect_errors(&runner);
    let result = runner
        .emit_boundary(&settle(), preview_builder(true))
        .await
        .unwrap();
    assert_eq!(*observed.lock(), vec![json!(1)]);
    assert!(
        errors
            .lock()
            .iter()
            .any(|e| e.error == "Invalid boundary entries: Entry missing not found"),
        "{:?}",
        errors.lock()
    );
    assert!(result.entries.is_empty());
    assert!(result.valid);
}

#[tokio::test(flavor = "multi_thread")]
async fn boundary_keeps_shared_mutations_made_before_a_handler_throws() {
    let dir = ExtDir::new();
    let (runner, _) = observing_runner(
        &dir,
        &[(
            "throws.js",
            r#"export default function(pi) {
	pi.on("agent_before_settle", (event) => {
		event.entries.push({ type: "custom", customType: "kept" });
		throw new Error("boundary failed");
	});
}"#,
        )],
    )
    .await;
    let errors = collect_errors(&runner);
    let result = runner
        .emit_boundary(&settle(), preview_builder(false))
        .await
        .unwrap();
    let entries = serde_json::to_value(&result.entries).unwrap();
    assert_eq!(entries[0]["type"], json!("custom"));
    assert_eq!(entries[0]["customType"], json!("kept"));
    assert_eq!(
        errors
            .lock()
            .iter()
            .map(|e| e.error.clone())
            .collect::<Vec<_>>(),
        ["boundary failed"]
    );
}

// ----------------------------------------------------------------------------
// tool_result chaining
// ----------------------------------------------------------------------------

fn tool_result_event(id: &str) -> ToolResultEvent {
    serde_json::from_value(json!({
        "toolName": "my_tool",
        "toolCallId": id,
        "input": {},
        "content": [{ "type": "text", "text": "base" }],
        "details": { "initial": true },
        "isError": false,
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_result_chains_content_modifications() {
    let dir = ExtDir::new();
    for (name, tag) in [("tool-result-1.ts", "ext1"), ("tool-result-2.ts", "ext2")] {
        dir.write(
            name,
            &format!(
                r#"export default function(pi) {{
	pi.on("tool_result", async (event) => ({{ content: [...event.content, {{ type: "text", text: "{tag}" }}] }}));
}}"#
            ),
        );
    }
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let chained = runner
        .emit_tool_result(&tool_result_event("call-1"))
        .await
        .unwrap()
        .expect("result");
    let content = serde_json::to_value(chained.content.expect("content")).unwrap();
    assert_eq!(content[0], json!({ "type": "text", "text": "base" }));
    let mut appended: Vec<String> = content.as_array().unwrap()[1..]
        .iter()
        .map(|c| c["text"].as_str().unwrap().to_string())
        .collect();
    appended.sort();
    assert_eq!(appended, ["ext1", "ext2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_result_preserves_previous_modifications_for_partial_patches() {
    let dir = ExtDir::new();
    dir.write(
        "tool-result-partial-1.ts",
        r#"export default function(pi) {
	pi.on("tool_result", async () => ({ content: [{ type: "text", text: "first" }], details: { source: "ext1" } }));
}"#,
    );
    dir.write(
        "tool-result-partial-2.ts",
        r#"export default function(pi) { pi.on("tool_result", async () => ({ isError: true })); }"#,
    );
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let chained = runner
        .emit_tool_result(&tool_result_event("call-2"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(chained).unwrap(),
        json!({ "content": [{ "type": "text", "text": "first" }], "details": { "source": "ext1" }, "isError": true })
    );
}

// ----------------------------------------------------------------------------
// Provider registration
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn bind_core_reports_invalid_queued_registrations() {
    let dir = ExtDir::new();
    let runtime = create_extension_runtime();
    let stream_simple: StreamSimpleFn = Arc::new(|_, _, _| panic!("should not run"));
    runtime
        .hooks
        .read()
        .clone()
        .register_provider(
            "broken-provider",
            ProviderConfig {
                stream_simple: Some(stream_simple),
                ..Default::default()
            },
            Some("/tmp/broken-extension.ts"),
        )
        .unwrap();
    let runner = runner_for(&dir, &[], runtime).await;
    let errors = collect_errors(&runner);
    bind(&runner);
    assert_eq!(
        errors
            .lock()
            .iter()
            .map(|e| format!("{}: {}", e.extension_path, e.error))
            .collect::<Vec<_>>(),
        [
            "/tmp/broken-extension.ts: Provider broken-provider: \"api\" is required when registering streamSimple."
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_bind_unregister_removes_all_queued_registrations() {
    let runtime = create_extension_runtime();
    let hooks = runtime.hooks.read().clone();
    hooks
        .register_provider("queued-provider", provider_model_config(), None)
        .unwrap();
    hooks
        .register_provider("queued-provider", provider_model_config(), None)
        .unwrap();
    assert_eq!(runtime.pending_provider_registrations.lock().len(), 2);
    hooks.unregister_provider("queued-provider", None).unwrap();
    assert!(runtime.pending_provider_registrations.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn post_bind_register_and_unregister_take_effect_immediately() {
    let dir = ExtDir::new();
    let runtime = create_extension_runtime();
    let registry = model_registry().await;
    let root = dir.root();
    let runner = ExtensionRunner::new(
        &[],
        runtime.clone(),
        &root,
        session_manager(&root),
        registry.clone(),
    );
    bind(&runner);
    assert!(runtime.pending_provider_registrations.lock().is_empty());

    runtime
        .hooks
        .read()
        .clone()
        .register_provider("instant-provider", provider_model_config(), None)
        .unwrap();
    assert!(runtime.pending_provider_registrations.lock().is_empty());
    let model = registry
        .find("instant-provider", "instant-model")
        .expect("model");
    assert_eq!(
        serde_json::to_value(&model.cost).unwrap()["tiers"],
        json!([{ "inputTokensAbove": 272000, "input": 2, "output": 3, "cacheRead": 0.2, "cacheWrite": 2.5 }])
    );

    runtime
        .hooks
        .read()
        .clone()
        .unregister_provider("instant-provider", None)
        .unwrap();
    assert!(registry.find("instant-provider", "instant-model").is_none());
}

// ----------------------------------------------------------------------------
// Command context
// ----------------------------------------------------------------------------

#[derive(Default)]
struct ForkRecorder {
    calls: Mutex<Vec<(String, Option<ForkPosition>)>>,
}

#[async_trait::async_trait]
impl ExtensionCommandContextActions for ForkRecorder {
    async fn wait_for_idle(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn new_session(
        &self,
        _options: NewSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        Ok(SessionReplaceOutcome { cancelled: false })
    }
    async fn fork(
        &self,
        entry_id: &str,
        options: ForkOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        self.calls
            .lock()
            .push((entry_id.to_string(), options.position));
        Ok(SessionReplaceOutcome { cancelled: false })
    }
    async fn navigate_tree(
        &self,
        _target_id: &str,
        _options: NavigateTreeOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        Ok(SessionReplaceOutcome { cancelled: false })
    }
    async fn switch_session(
        &self,
        _session_path: &str,
        _options: SwitchSessionOptions,
    ) -> anyhow::Result<SessionReplaceOutcome> {
        Ok(SessionReplaceOutcome { cancelled: false })
    }
    async fn reload(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn command_context_passes_fork_options_through() {
    let dir = ExtDir::new();
    let runner = runner_for(&dir, &[], create_extension_runtime()).await;
    let recorder = Arc::new(ForkRecorder::default());
    runner.bind_command_context(Some(recorder.clone()));
    let ctx = runner.create_command_context();
    ctx.fork("entry-1", ForkOptions::default()).await.unwrap();
    ctx.fork(
        "entry-2",
        ForkOptions {
            position: Some(ForkPosition::At),
            with_session: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        *recorder.calls.lock(),
        vec![
            ("entry-1".to_string(), None),
            ("entry-2".to_string(), Some(ForkPosition::At))
        ]
    );
}

// ----------------------------------------------------------------------------
// Event subscriptions (#8967)
// ----------------------------------------------------------------------------

async fn subscription_runner(f: ExtensionFactory) -> (ExtDir, ExtensionRef, Arc<ExtensionRunner>) {
    let dir = ExtDir::new();
    let runtime = create_extension_runtime();
    let extension = load_extension_from_factory(&f, &dir.root(), bus(), runtime.clone(), None)
        .await
        .unwrap();
    let runner = runner_for(&dir, std::slice::from_ref(&extension), runtime).await;
    (dir, extension, runner)
}

type Cell = Arc<OnceLock<Unsubscribe>>;

fn stop(cell: &Cell) {
    (cell.get().expect("registered"))();
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_allow_self_removal() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let (_dir, _ext, runner) = subscription_runner(factory(move |pi| {
        let me: Cell = Arc::default();
        let (c2, me2) = (c.clone(), me.clone());
        let _ = me.set(pi.on(
            "agent_end",
            handler(move || {
                c2.lock().push("A");
                stop(&me2);
                Box::pin(async {})
            }),
        )?);
        pi.on("agent_end", push(&c, "B"))?;
        Ok(())
    }))
    .await;
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B"]);
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B", "B"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_remove_duplicate_registrations_independently() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let unsubscribers: Arc<Mutex<Vec<Unsubscribe>>> = Arc::default();
    let (c, u) = (calls.clone(), unsubscribers.clone());
    let (_dir, extension, runner) = subscription_runner(factory(move |pi| {
        let shared = push(&c, "shared");
        u.lock().push(pi.on("agent_end", shared.clone())?);
        u.lock().push(pi.on("agent_end", push(&c, "B"))?);
        u.lock().push(pi.on("agent_end", shared)?);
        Ok(())
    }))
    .await;
    let [stop_first, stop_b, stop_second] =
        <[Unsubscribe; 3]>::try_from(unsubscribers.lock().clone())
            .ok()
            .unwrap();

    stop_second();
    stop_second();
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["shared", "B"]);

    stop_first();
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["shared", "B", "B"]);

    stop_b();
    assert!(!extension.read().handlers.contains_key("agent_end"));
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_keep_removed_pending_handlers_in_the_current_dispatch() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let (_dir, _ext, runner) = subscription_runner(factory(move |pi| {
        let stop_b: Cell = Arc::default();
        let (c2, s2) = (c.clone(), stop_b.clone());
        pi.on(
            "agent_end",
            handler(move || {
                c2.lock().push("A");
                stop(&s2);
                Box::pin(async {})
            }),
        )?;
        let _ = stop_b.set(pi.on("agent_end", push(&c, "B"))?);
        pi.on("agent_end", push(&c, "C"))?;
        Ok(())
    }))
    .await;
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B", "C"]);
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B", "C", "A", "C"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_defer_registrations_made_during_dispatch() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let (_dir, _ext, runner) = subscription_runner(factory(move |pi| {
        let (c2, pi2) = (c.clone(), pi.clone());
        pi.on(
            "agent_end",
            handler(move || {
                c2.lock().push("A");
                pi2.on("agent_end", push(&c2, "C")).unwrap();
                Box::pin(async {})
            }),
        )?;
        pi.on("agent_end", push(&c, "B"))?;
        Ok(())
    }))
    .await;
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B"]);
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "B", "A", "B", "C"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriptions_use_a_fresh_handler_list_for_nested_dispatches() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner_cell: Arc<OnceLock<Weak<ExtensionRunner>>> = Arc::default();
    let (c, rc) = (calls.clone(), runner_cell.clone());
    let (_dir, _ext, runner) = subscription_runner(factory(move |pi| {
        let stop_a: Cell = Arc::default();
        let stop_b: Cell = Arc::default();
        let (c2, pi2, rc2, sa, sb) = (
            c.clone(),
            pi.clone(),
            rc.clone(),
            stop_a.clone(),
            stop_b.clone(),
        );
        let _ = stop_a.set(pi.on(
            "agent_end",
            handler(move || {
                c2.lock().push("A");
                stop(&sa);
                stop(&sb);
                pi2.on("agent_end", push(&c2, "C")).unwrap();
                let runner = rc2.get().and_then(Weak::upgrade).expect("runner");
                Box::pin(async move {
                    runner.emit(agent_end()).await.unwrap();
                })
            }),
        )?);
        let _ = stop_b.set(pi.on("agent_end", push(&c, "B"))?);
        Ok(())
    }))
    .await;
    let _ = runner_cell.set(Arc::downgrade(&runner));
    runner.emit(agent_end()).await.unwrap();
    assert_eq!(*calls.lock(), ["A", "C", "B"]);
}

// ----------------------------------------------------------------------------
// hasHandlers / before_provider_headers
// ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn has_handlers_reports_registered_events() {
    let dir = ExtDir::new();
    dir.write(
        "handler.ts",
        r#"export default function(pi) { pi.on("tool_call", async () => undefined); }"#,
    );
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    assert!(runner.has_handlers("tool_call"));
    assert!(!runner.has_handlers("agent_end"));
}

fn headers(pairs: &[(&str, &str)]) -> pi_ai::types::ProviderHeaders {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Some(v.to_string())))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn before_provider_headers_mutates_in_place_and_preserves_existing() {
    let dir = ExtDir::new();
    dir.write(
        "headers.ts",
        r#"export default function(pi) { pi.on("before_provider_headers", (event) => { event.headers["X-Turn-Index"] = "3"; }); }"#,
    );
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    assert!(runner.has_handlers("before_provider_headers"));
    let out = runner
        .emit_before_provider_headers(headers(&[("User-Agent", "kimchi/1.0")]))
        .await
        .unwrap();
    assert_eq!(out.get("X-Turn-Index"), Some(&Some("3".to_string())));
    assert_eq!(out.get("User-Agent"), Some(&Some("kimchi/1.0".to_string())));
}

#[tokio::test(flavor = "multi_thread")]
async fn before_provider_headers_isolates_throwing_handlers() {
    let dir = ExtDir::new();
    dir.write(
        "a-throwing.ts",
        r#"export default function(pi) { pi.on("before_provider_headers", () => { throw new Error("header handler boom"); }); }"#,
    );
    dir.write(
        "b-good.ts",
        r#"export default function(pi) { pi.on("before_provider_headers", (event) => { event.headers["X-Good"] = "yes"; }); }"#,
    );
    let result = dir.discover().await;
    let runner = dir.runner(&result).await;
    let errors = collect_errors(&runner);
    let out = runner
        .emit_before_provider_headers(headers(&[("User-Agent", "x")]))
        .await
        .unwrap();
    assert_eq!(out.get("X-Good"), Some(&Some("yes".to_string())));
    assert_eq!(out.get("User-Agent"), Some(&Some("x".to_string())));
    let errors = errors.lock();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].event, "before_provider_headers");
    assert!(errors[0].error.contains("header handler boom"));
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_transformer_receives_persisted_identity_and_undefined_live_metadata() {
    let dir = ExtDir::new();
    dir.write(
        "markdown-context.ts",
        r#"export default function(pi) {
            pi.registerMarkdownTransformer((markdown, context) =>
                JSON.stringify({
                    markdown,
                    messageType: context.messageType,
                    isStreaming: context.isStreaming,
                    availableWidth: context.availableWidth,
                    messageId: context.messageId,
                    timestamp: context.timestamp,
                    live: context.messageId === undefined && context.timestamp === undefined
                }));
        }"#,
    );
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let runner = dir.runner(&result).await;
    let transformers = runner.get_markdown_transformers();
    for metadata in [json!({}), json!({"messageId": "entry-abc", "timestamp": "2025-01-15T10:30:00.000Z"})] {
        let mut wire = json!({"messageType": "assistant-thinking", "isStreaming": false, "availableWidth": 78});
        wire.as_object_mut().unwrap().extend(metadata.as_object().unwrap().clone());
        let context: MarkdownTransformContext = serde_json::from_value(wire.clone()).unwrap();
        let actual: Value = serde_json::from_str(&transformers[0]("reasoning", &context)).unwrap();
        wire["markdown"] = json!("reasoning");
        wire["live"] = json!(metadata.get("messageId").is_none());
        assert_eq!(actual, wire);
    }
}
