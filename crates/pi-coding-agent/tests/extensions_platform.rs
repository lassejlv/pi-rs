//! Extension APIs that reach into pi's process-wide state like the TS modules extensions share
//! with pi: `setKeybindings()` (pi-tui's global keybindings), `getNativeClipboard()` (pi-tui's
//! native clipboard) and pi-ai telemetry for JavaScript `Models` collections.

mod extensions_support;
mod support;

use std::path::PathBuf;
use std::sync::Arc;

use extensions_support::*;
use pi_ai::types::{Context, Message, StopReason, StreamOptions, UserContent, UserMessage};
use pi_coding_agent::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions, ResourceLoader};
use pi_telemetry::{InMemoryTelemetryContext, TelemetryContextExt};
use serde_json::{Value, json};

/// Load `source` as the only extension and return its single tool's description as JSON.
async fn run_extension(source: &str) -> Value {
    let dir = ExtDir::new();
    dir.write("platform.ts", source);
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:#?}", result.errors);
    let extension = result.extensions[0].read();
    serde_json::from_str(&extension.tools["out"].definition.description).unwrap()
}

/// TS: extensions share pi-tui's module state, so `setKeybindings()` replaces the bindings of the
/// whole app (pi's editor and selectors read pi-tui's global manager).
#[tokio::test(flavor = "multi_thread")]
async fn set_keybindings_replaces_the_global_keybindings() {
    let previous = pi_tui::keybindings::get_keybindings();
    let out = run_extension(
        r#"
        import { KeybindingsManager, TUI_KEYBINDINGS, setKeybindings, getKeybindings } from "@earendil-works/pi-tui";
        export default function (pi) {
            const manager = new KeybindingsManager(TUI_KEYBINDINGS, { "tui.select.up": "ctrl+k" });
            setKeybindings(manager);
            const out = { same: getKeybindings() === manager, js: getKeybindings().matches("\x0b", "tui.select.up") };
            // Later changes to the installed manager reach pi too.
            manager.setUserBindings({ "tui.select.up": ["ctrl+k", "ctrl+p"], "tui.select.down": "ctrl+j" });
            pi.registerTool({ name: "out", label: "out", description: JSON.stringify(out), parameters: { type: "object", properties: {} }, execute: async () => ({ content: [] }) });
        }
        "#,
    )
    .await;
    let global = pi_tui::keybindings::get_keybindings();
    let result = (
        out,
        global.matches("\x0b", "tui.select.up"),
        global.matches("\x10", "tui.select.up"),
        global.matches("\x0a", "tui.select.down"),
        global.get_keys("tui.select.down"),
    );
    pi_tui::keybindings::set_keybindings(previous);
    assert_eq!(result.0, json!({ "same": true, "js": true }));
    assert!(result.1, "ctrl+k selects up in pi's global keybindings");
    assert!(result.2, "setUserBindings() on the installed manager is mirrored");
    assert!(result.3);
    assert_eq!(result.4, vec!["ctrl+j".to_string()]);
}

/// `getNativeClipboard()` is pi-tui's native clipboard (arboard), present exactly where
/// `pi_tui::native_platform::get_native_clipboard()` is (TS: where the native addon loads).
#[tokio::test(flavor = "multi_thread")]
async fn native_clipboard_matches_pi_tui() {
    let out = run_extension(
        r#"
        import { getNativeClipboard } from "@earendil-works/pi-tui";
        export default function (pi) {
            const clipboard = getNativeClipboard();
            const out = clipboard === undefined
                ? { present: false }
                : { present: true, methods: ["getText", "getImage", "getFilePaths"].map((m) => typeof clipboard[m]), setText: typeof clipboard.setText };
            pi.registerTool({ name: "out", label: "out", description: JSON.stringify(out), parameters: { type: "object", properties: {} }, execute: async () => ({ content: [] }) });
        }
        "#,
    )
    .await;
    match pi_tui::native_platform::get_native_clipboard() {
        None => assert_eq!(out, json!({ "present": false })),
        Some(native) => {
            let set_text = if native.can_set_text() { "function" } else { "undefined" };
            assert_eq!(out, json!({ "present": true, "methods": ["function", "function", "function"], "setText": set_text }));
        }
    }
}

const TELEMETRY_EXTENSION: &str = r#"
import { createModels, createProvider, fauxAssistantMessage, fauxProvider } from "@earendil-works/pi-ai";
export default function (pi) {
    const inner = fauxProvider({ provider: "js-inner", models: [{ id: "inner-1" }] });
    inner.setResponses([fauxAssistantMessage("inner reply")]);
    const models = createModels();
    models.setProvider(inner.provider);
    const outer = fauxProvider({ provider: "js-outer", models: [{ id: "outer-1" }] });
    pi.registerProvider(createProvider({
        id: "js-outer",
        auth: { apiKey: { name: "Outer", resolve: async () => ({ auth: {} }) } },
        models: outer.models,
        api: {
            stream: (_model, context, options) => models.stream(inner.getModel(), context, options),
            streamSimple: (_model, context, options) => models.streamSimple(inner.getModel(), context, options),
        },
    }));
}
"#;

/// pi-ai `observeModelRequest`: a JavaScript `Models` collection that forwards the stream options
/// pi passed to its provider records its request under pi's telemetry parent, like TS.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn js_models_requests_are_observed_by_pi_ai_telemetry() {
    check_js_models_telemetry(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn js_models_requests_accept_a_root_telemetry_context() {
    check_js_models_telemetry(true).await;
}

async fn check_js_models_telemetry(root_parent: bool) {
    let dir = std::env::temp_dir().join(format!("pi-ext-telemetry-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("extension.ts");
    std::fs::write(&path, TELEMETRY_EXTENSION).unwrap();
    let loader = Arc::new(DefaultResourceLoader::new(&DefaultResourceLoaderOptions {
        cwd: dir.display().to_string(),
        agent_dir: dir.display().to_string(),
        additional_extension_paths: Some(vec![path.display().to_string()]),
        no_skills: Some(true),
        no_prompt_templates: Some(true),
        no_context_files: Some(true),
        ..Default::default()
    }));
    loader.reload(None).await.unwrap();
    assert!(loader.get_extensions().errors.is_empty(), "{:#?}", loader.get_extensions().errors);
    let harness = support::create_harness(support::HarnessOptions { resource_loader: Some(loader), ..Default::default() })
        .await
        .unwrap();
    harness.session.bind_extensions(&Default::default()).await.unwrap();
    let runtime = harness.model_runtime.clone();
    let model = runtime.get_model("js-outer", "outer-1").expect("the extension registers its provider");

    let telemetry = InMemoryTelemetryContext::new();
    let request = move |parent: pi_telemetry::ContextRef| async move {
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage { content: UserContent::Text("hi".into()), timestamp: 0, ..Default::default() })],
            tools: None,
        };
        let options = StreamOptions { telemetry: Some(parent), ..Default::default() };
        runtime.complete_simple(model, context, Some(options.into())).await
    };
    let message = if root_parent {
        request(telemetry.clone()).await
    } else {
        telemetry.start_span("caller", |parent| request(parent)).await
    };
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    for _ in 0..200 {
        if telemetry.get_spans().iter().all(|s| s.settled) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let spans = telemetry.get_spans();
    let requests: Vec<(String, Value, bool)> = spans
        .iter()
        .filter(|s| s.name == "pi.ai.request")
        .map(|s| {
            let attributes = serde_json::to_value(&s.attributes).unwrap();
            (attributes["provider"].as_str().unwrap_or_default().to_string(), attributes["stop"].clone(), s.settled)
        })
        .collect();
    // The extension's nested request (JavaScript Models), recorded by pi-ai's Rust telemetry.
    assert!(requests.contains(&("js-inner".to_string(), json!("stop"), true)), "{requests:?}");
    let inner = spans.iter().find(|s| s.name == "pi.ai.request" && serde_json::to_value(&s.attributes).unwrap()["provider"] == "js-inner").unwrap();
    if root_parent {
        assert_eq!(inner.parent_id, None, "the root context reaches the JavaScript request");
    } else {
        let caller = spans.iter().find(|s| s.name == "caller").unwrap();
        assert_eq!(inner.parent_id, Some(caller.id), "the JS request is a child of pi's telemetry parent");
    }
    let events: Vec<&str> = inner.events.iter().map(|e| e.name.as_str()).collect();
    assert!(events.contains(&"pi.ai.first_chunk") && events.contains(&"pi.ai.block"), "{events:?}");
    harness.cleanup();
    let _ = std::fs::remove_dir_all(dir);
}

const LAYOUT_EXTENSION: &str = r#"
import { ScrollView, Text, VStack, HStack } from "@earendil-works/pi-tui";
export default function (pi) {
    const body = new Text(Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n"), 0, 0);
    const scroll = new ScrollView(body, { primary: true, follow: "end", scrollbar: "hidden" });
    globalThis.__layoutScroll = scroll;
    const side = new HStack([new Text("L", 0, 0), { component: new Text("R", 0, 0), grow: 1 }], { gap: 1 });
    const root = new VStack([
        new Text("header", 0, 0),
        { component: scroll, basis: 0, grow: 1 },
        { component: new Text("hidden when narrow", 0, 0), visible: (viewport) => viewport.width >= 30 },
        side,
    ]);
    pi.registerTool({
        name: "layout",
        label: "layout",
        description: "layout",
        parameters: { type: "object", properties: {} },
        execute: async (_id, params) => {
            if (params.action === "end") scroll.scrollToEnd();
            const out = { scrollTop: scroll.scrollTop, following: scroll.isFollowingEnd, viewport: scroll.viewportHeight };
            return { content: [{ type: "text", text: JSON.stringify(out) }], details: {} };
        },
        renderCall: () => root,
    });
}
"#;

/// TS `[LAYOUT_NODE]()`: JavaScript VStack/HStack/ScrollView take part in pi-tui's fullscreen layout
/// (flex sizing, visibility predicates, a primary scroll view that follows the end and scrolls with
/// the wheel), and the JavaScript ScrollView reads the state the layout gives it.
#[tokio::test(flavor = "multi_thread")]
async fn js_stacks_and_scroll_views_use_the_fullscreen_layout() {
    use pi_coding_agent::core::extensions::types::ToolRenderContext;
    let dir = ExtDir::new();
    dir.write("layout.ts", LAYOUT_EXTENSION);
    let result = dir.discover().await;
    assert!(result.errors.is_empty(), "{:#?}", result.errors);
    let tool = result.extensions[0].read().tools["layout"].definition.clone();
    let theme = pi_coding_agent::modes::interactive::theme::theme::theme();
    let render_ctx = ToolRenderContext {
        args: json!({}),
        tool_call_id: "call".into(),
        invalidate: Arc::new(|| {}),
        last_component: None,
        state: Default::default(),
        cwd: dir.root(),
        execution_started: false,
        args_complete: true,
        is_partial: false,
        expanded: false,
        show_images: false,
        is_error: false,
        duration_ms: None,
        output_pad: 0,
    };
    let root = (tool.render_call.clone().expect("renderCall"))(&json!({}), &theme, &render_ctx);
    assert!(root.layout_node().is_some(), "a JS VStack has a layout node");
    let plain = |frame: &pi_tui::layout::LayoutFrame| -> Vec<String> {
        frame.lines.iter().map(|l| pi_coding_agent::utils::ansi::strip_ansi(l).unwrap().trim_end().to_string()).collect()
    };
    let render = |width: usize| {
        let root = root.clone();
        tokio::task::block_in_place(move || pi_tui::layout::render_layout_frame(&root, width, 8, Arc::new(|| {})))
    };
    let execute = |action: &str| {
        let execute = tool.execute.clone();
        let args = json!({ "action": action });
        async move {
            let result = execute("call".into(), args, None, None, None).await.unwrap();
            let text = serde_json::to_value(&result.content).unwrap()[0]["text"].as_str().unwrap().to_string();
            serde_json::from_str::<Value>(&text).unwrap()
        }
    };

    // 8 rows: header, the scroll view grows into 5 rows (following the end), the visible line, the HStack.
    let lines = plain(&render(40));
    assert_eq!(lines[..7], ["header", "line 16", "line 17", "line 18", "line 19", "line 20", "hidden when narrow"]);
    // HStack: both Texts pad to the full width (intrinsic 40 each) and shrink side by side.
    assert!(lines[7].starts_with("L ") && lines[7].ends_with('R'), "{:?}", lines[7]);
    assert_eq!(execute("read").await, json!({ "scrollTop": 15, "following": true, "viewport": 5 }));

    // Narrow viewport: the predicate hides an entry and the scroll view takes the row.
    let frame = render(20);
    assert_eq!(plain(&frame)[1..7], ["line 15", "line 16", "line 17", "line 18", "line 19", "line 20"]);

    // Wheel scrolling goes to the primary scroll view; the JS ScrollView sees the new state.
    let frame = render(40);
    let primary = frame.primary_scroll_view.clone().expect("the JS ScrollView is the primary scroll view");
    primary.scroll_by(-10);
    assert_eq!(plain(&render(40))[1..6], ["line 6", "line 7", "line 8", "line 9", "line 10"]);
    assert_eq!(execute("read").await, json!({ "scrollTop": 5, "following": false, "viewport": 5 }));

    // JS scrolling reaches the layout.
    assert_eq!(execute("end").await, json!({ "scrollTop": 15, "following": true, "viewport": 5 }));
    assert_eq!(plain(&render(40))[1], "line 16");
}
