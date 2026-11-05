//! UI bridging: JS components as Rust [`Component`]s, Rust objects (TUI, theme, keybindings,
//! Rust components) as JS proxies, and the renderer / factory closures built from JS functions.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_jsrt::JsHandle;
use pi_tui::autocomplete::{AutocompleteProvider, AutocompleteSuggestions, CompletionResult, SuggestOptions};
use pi_tui::components::editor::{Editor, EditorComponent, EditorOptions, EditorTheme, TextCallback};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use pi_tui::components::select_list::SelectListTheme;
use pi_tui::tui::{
    Component, ComponentRef, MouseButton, MouseDispatchTarget, MouseEvent, MouseEventType, MouseResult, OverlayAnchor, OverlayHandle,
    OverlayMargin, OverlayOptions, SizeValue, Tui,
};
use pi_tui::StyleFn;
use pi_tui::components::scroll_view::{Overscroll, ScrollFollow, ScrollView, ScrollViewOptions, ScrollViewScrollToOptions, ScrollViewScrollbar};
use pi_tui::layout_node::{
    LayoutNode, LayoutViewport, ScrollLayoutNode, StackAlign, StackBasis, StackLayoutEntry, StackLayoutNode, StackType,
    StackVisibleFn,
};
use serde_json::{Value, json};

use super::api::{arg, autocomplete_item, autocomplete_item_json, custom_entry_json, opt, post_call, rust_marker, str_arg};
use super::{Bridge, Registry, js_error};
use crate::core::extensions::types::*;
use crate::core::footer_data_provider::ReadonlyFooterDataProvider;
use crate::core::keybindings::{AppKeybinding, KeybindingsManager};
use crate::modes::interactive::components::custom_editor::{CustomEditor, CustomEditorOptions};
use crate::modes::interactive::theme::theme::Theme;

/// A value passed to `done()` of a JS custom UI component.
pub struct JsUiValue(pub Value);

/// Renderer state of a JS tool row: a handle to the JS `context.state` object.
struct JsRenderState(JsHandle);

/// Renderers returned by `next()` in a tool renderer resolver, kept for JS to call.
#[derive(Clone)]
pub(crate) struct RendererEntry {
    render_call: Option<ToolRenderCallFn>,
    render_result: Option<ToolRenderResultFn>,
}

/// A Rust component created or exposed for JS (`rc` host calls).
#[derive(Clone)]
pub(crate) enum RustComp {
    Input(Arc<Input>),
    Editor(Arc<Editor>),
    /// coding-agent `CustomEditor` (app-level keybindings) behind JS `CustomEditor`.
    CustomEditor(Arc<CustomEditor>),
    Other(ComponentRef),
}

impl RustComp {
    fn component(&self) -> ComponentRef {
        match self {
            Self::Input(c) => c.clone(),
            Self::Editor(c) => c.clone(),
            Self::CustomEditor(c) => c.clone(),
            Self::Other(c) => c.clone(),
        }
    }

    /// The pi-tui editor of an editor-backed component.
    fn editor(&self) -> Option<&Arc<Editor>> {
        match self {
            Self::Editor(e) => Some(e),
            Self::CustomEditor(c) => Some(c.editor()),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ComponentRegistries {
    pub(crate) themes: Registry<Arc<Theme>>,
    theme_ids: Mutex<HashMap<usize, u64>>,
    pub(crate) tuis: Registry<Arc<Tui>>,
    tui_ids: Mutex<HashMap<usize, u64>>,
    pub(crate) keybindings: Registry<Arc<KeybindingsManager>>,
    keybinding_ids: Mutex<HashMap<usize, u64>>,
    pub(crate) overlays: Registry<OverlayHandle>,
    pub(crate) footers: Registry<ReadonlyFooterDataProvider>,
    pub(crate) rust: Registry<RustComp>,
    pub(crate) renderers: Registry<RendererEntry>,
    pub(crate) render_contexts: Registry<ToolRenderContext>,
    render_context_ids: Mutex<HashMap<String, u64>>,
    pub(crate) markdown_themes: Registry<MarkdownTheme>,
    pub(crate) autocomplete: Registry<Arc<dyn AutocompleteProvider>>,
    /// One wrapper per JavaScript layout child (`layoutId`), reused across layouts.
    layout_children: Mutex<HashMap<u64, std::sync::Weak<JsComponent>>>,
    /// Rust `ScrollView`s that lay out JavaScript ScrollViews (`sv` host calls), by mirror id.
    scroll_mirrors: Mutex<HashMap<u64, std::sync::Weak<ScrollView>>>,
}

impl<T: Clone> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn keyed_id<T: Clone>(registry: &Registry<T>, ids: &Mutex<HashMap<usize, u64>>, key: usize, value: T) -> u64 {
    let mut ids = ids.lock();
    if let Some(id) = ids.get(&key) {
        return *id;
    }
    let id = registry.insert(value);
    ids.insert(key, id);
    id
}

pub(crate) fn theme_id(bridge: &Bridge, theme: &Arc<Theme>) -> u64 {
    let key = Arc::as_ptr(theme) as usize;
    keyed_id(&bridge.components.themes, &bridge.components.theme_ids, key, theme.clone())
}

pub(crate) fn tui_id(bridge: &Bridge, tui: &Arc<Tui>) -> u64 {
    let key = Arc::as_ptr(tui) as usize;
    keyed_id(&bridge.components.tuis, &bridge.components.tui_ids, key, tui.clone())
}

pub(crate) fn keybindings_id(bridge: &Bridge, kb: &Arc<KeybindingsManager>) -> u64 {
    let key = Arc::as_ptr(kb) as usize;
    keyed_id(&bridge.components.keybindings, &bridge.components.keybinding_ids, key, kb.clone())
}

// ---------------------------------------------------------------------------
// JS components
// ---------------------------------------------------------------------------

/// A JS component (an object with `render(width)`) as a pi-tui [`Component`].
pub struct JsComponent {
    handle: JsHandle,
    bridge: Arc<Bridge>,
    handles_input: bool,
    focusable: bool,
    wants_key_release: bool,
    has_invalidate: bool,
    has_dispose: bool,
    has_mouse: bool,
    /// Rust component behind a JS subclass of a Rust-backed class (e.g. CustomEditor).
    rust_id: Option<u64>,
    /// Has `[LAYOUT_NODE]()` (JS stacks and scroll views).
    has_layout_node: bool,
    /// The Rust ScrollView laying out this JS ScrollView in the fullscreen layout.
    scroll_mirror: Mutex<Option<Arc<ScrollView>>>,
    /// Child wrappers of the last layout node (kept alive with this component).
    layout_children: Mutex<Vec<ComponentRef>>,
    focused: AtomicBool,
    last_lines: Mutex<Vec<String>>,
}

impl JsComponent {
    fn new(bridge: Arc<Bridge>, handle: JsHandle, desc: &Value) -> Self {
        Self {
            handle,
            bridge,
            handles_input: desc["handlesInput"].as_bool().unwrap_or(false),
            focusable: desc["focusable"].as_bool().unwrap_or(false),
            wants_key_release: desc["wantsKeyRelease"].as_bool().unwrap_or(false),
            has_invalidate: desc["hasInvalidate"].as_bool().unwrap_or(false),
            has_dispose: desc["hasDispose"].as_bool().unwrap_or(false),
            has_mouse: desc["hasMouse"].as_bool().unwrap_or(false),
            rust_id: desc["rid"].as_u64(),
            has_layout_node: desc["hasLayoutNode"].as_bool().unwrap_or(false),
            scroll_mirror: Mutex::new(None),
            layout_children: Mutex::new(Vec::new()),
            focused: AtomicBool::new(false),
            last_lines: Mutex::new(Vec::new()),
        }
    }

    /// The JS object.
    pub fn handle(&self) -> &JsHandle {
        &self.handle
    }

    /// The Rust component behind a JS subclass of a Rust-backed class, if any.
    pub fn rust_component(&self) -> Option<ComponentRef> {
        self.rust_id.and_then(|id| self.bridge.components.rust.get(id)).map(|c| c.component())
    }

    fn call(&self, method: &str, args: Vec<Value>) -> pi_jsrt::Result<Value> {
        self.handle.call_method_blocking(method, args)
    }
}

impl Component for JsComponent {
    fn render(&self, width: usize) -> Vec<String> {
        match self.call("render", vec![json!(width)]) {
            Ok(Value::Array(lines)) => {
                let lines: Vec<String> = lines
                    .into_iter()
                    .map(|l| match l {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .collect();
                *self.last_lines.lock() = lines.clone();
                lines
            }
            Ok(_) => Vec::new(),
            Err(pi_jsrt::Error::Deadlock) => self.last_lines.lock().clone(),
            Err(e) => {
                let message = format!("[extension render error: {}]", js_error(e));
                vec![pi_tui::utils::truncate_to_width(&message, width, "...", false)]
            }
        }
    }

    fn handle_input(&self, data: &str) {
        if self.handles_input {
            let _ = self.call("handleInput", vec![json!(data)]);
        }
    }

    fn handles_input(&self) -> bool {
        self.handles_input
    }

    fn wants_key_release(&self) -> bool {
        self.wants_key_release
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        if !self.has_mouse {
            return None;
        }
        let result = self.bridge.glue_call_blocking("invokeHandleMouse", vec![self.handle.to_value(), mouse_event_json(event)]).ok()?;
        mouse_result_from(&self.bridge, &result)
    }

    fn invalidate(&self) {
        if self.has_invalidate {
            let _ = self.call("invalidate", vec![]);
        }
    }

    fn is_focusable(&self) -> bool {
        self.focusable
    }

    fn set_focused(&self, focused: bool) {
        self.focused.store(focused, Ordering::SeqCst);
        if self.focusable {
            let _ = self.bridge.glue_call_blocking("setProp", vec![self.handle.to_value(), json!("focused"), json!(focused)]);
        }
    }

    fn focused(&self) -> bool {
        self.focused.load(Ordering::SeqCst)
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        if !self.has_layout_node {
            return None;
        }
        let desc = self.bridge.glue_call_blocking("describeLayoutNode", vec![self.handle.to_value()]).ok()?;
        let node = layout_node_from(&self.bridge, &desc, &self.scroll_mirror)?;
        // The children (and a scroll view's mirror inside its child wrapper) live as long as this
        // component, so their wrappers and scroll state persist between layouts.
        *self.layout_children.lock() = match &node {
            LayoutNode::Stack(stack) => stack.entries.iter().map(|e| e.component.clone()).collect(),
            LayoutNode::Scroll(scroll) => vec![scroll.component.clone()],
        };
        Some(node)
    }
}

/// A layout child: Rust components directly, JavaScript components through one wrapper per
/// JavaScript object (its `layoutId`). The parent owns the child, so wrappers never dispose it.
fn layout_child(bridge: &Arc<Bridge>, desc: &Value) -> Option<ComponentRef> {
    if desc.get("$piComponent").is_some() {
        return component_from_value(bridge, desc);
    }
    let handle = bridge.handle(&desc["$piJsComponent"])?;
    let Some(id) = desc["layoutId"].as_u64() else {
        let mut component = JsComponent::new(bridge.clone(), handle, desc);
        component.has_dispose = false;
        return Some(Arc::new(component));
    };
    let mut children = bridge.components.layout_children.lock();
    if let Some(existing) = children.get(&id).and_then(std::sync::Weak::upgrade) {
        return Some(existing);
    }
    if children.len() > 64 {
        children.retain(|_, weak| weak.strong_count() > 0);
    }
    let mut component = JsComponent::new(bridge.clone(), handle, desc);
    component.has_dispose = false;
    let component = Arc::new(component);
    children.insert(id, Arc::downgrade(&component));
    Some(component)
}

fn layout_size(value: &Value) -> Option<usize> {
    value.as_f64().filter(|v| v.is_finite()).map(|v| v.max(0.0) as usize)
}

/// A JavaScript layout node (`describeLayoutNode` in bridge.js) as a pi-tui [`LayoutNode`].
fn layout_node_from(bridge: &Arc<Bridge>, desc: &Value, mirror_slot: &Mutex<Option<Arc<ScrollView>>>) -> Option<LayoutNode> {
    match desc["type"].as_str()? {
        "scroll" => {
            let child = layout_child(bridge, &desc["component"])?;
            let existing = desc["mirror"]
                .as_u64()
                .and_then(|id| bridge.components.scroll_mirrors.lock().get(&id).and_then(std::sync::Weak::upgrade));
            // Adopt the handles of the description (released when unused).
            let js_state = bridge.handle(&desc["state"]);
            let state = match existing {
                Some(state) => {
                    drop((bridge.handle(&desc["init"]["trackStyle"]), bridge.handle(&desc["init"]["thumbStyle"])));
                    state
                }
                None => {
                    let state = new_scroll_mirror(bridge, child.clone(), &desc["init"]);
                    let id = super::next_id();
                    bridge.components.scroll_mirrors.lock().insert(id, Arc::downgrade(&state));
                    if let Some(js_state) = &js_state {
                        let _ = bridge.glue_call_blocking("attachScrollMirror", vec![js_state.to_value(), json!(id)]);
                    }
                    state
                }
            };
            *mirror_slot.lock() = Some(state.clone());
            Some(LayoutNode::Scroll(ScrollLayoutNode { component: child, state }))
        }
        kind @ ("vstack" | "hstack") => {
            let entries = desc["entries"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            let component = layout_child(bridge, &entry["component"])?;
                            let visible: Option<StackVisibleFn> = bridge.handle(&entry["visible"]).map(|handle| {
                                Arc::new(move |viewport: LayoutViewport| {
                                    handle
                                        .call_blocking(vec![json!({ "width": viewport.width, "height": viewport.height })])
                                        .map(|v| v.as_bool() == Some(true))
                                        .unwrap_or(true)
                                }) as StackVisibleFn
                            });
                            Some(StackLayoutEntry {
                                component,
                                basis: Some(layout_size(&entry["basis"]).map_or(StackBasis::Auto, StackBasis::Size)),
                                grow: layout_size(&entry["grow"]),
                                shrink: layout_size(&entry["shrink"]),
                                min_size: layout_size(&entry["minSize"]),
                                max_size: layout_size(&entry["maxSize"]),
                                visible,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(LayoutNode::Stack(StackLayoutNode {
                kind: if kind == "vstack" { StackType::VStack } else { StackType::HStack },
                entries,
                gap: layout_size(&desc["gap"]).unwrap_or(0),
                align: match desc["align"].as_str() {
                    Some("start") => StackAlign::Start,
                    Some("center") => StackAlign::Center,
                    Some("end") => StackAlign::End,
                    _ => StackAlign::Stretch,
                },
            }))
        }
        _ => None,
    }
}

fn parse_scrollbar(value: &Value) -> Option<ScrollViewScrollbar> {
    match value.as_str()? {
        "hidden" => Some(ScrollViewScrollbar::Hidden),
        "auto" => Some(ScrollViewScrollbar::Auto),
        "always" => Some(ScrollViewScrollbar::Always),
        _ => None,
    }
}

fn scrollbar_name(scrollbar: ScrollViewScrollbar) -> &'static str {
    match scrollbar {
        ScrollViewScrollbar::Hidden => "hidden",
        ScrollViewScrollbar::Auto => "auto",
        ScrollViewScrollbar::Always => "always",
    }
}

/// A Rust ScrollView starting from a JavaScript ScrollView's options and state (`__svInit()`).
fn new_scroll_mirror(bridge: &Arc<Bridge>, child: ComponentRef, init: &Value) -> Arc<ScrollView> {
    let options = ScrollViewOptions {
        follow: Some(if init["follow"].as_str() == Some("end") { ScrollFollow::End } else { ScrollFollow::None }),
        primary: init["primary"].as_bool(),
        overscroll: Some(if init["overscroll"].as_str() == Some("contain") { Overscroll::Contain } else { Overscroll::Chain }),
        scrollbar: parse_scrollbar(&init["scrollbar"]),
        scrollbar_track_style: js_style_fn(bridge, &init["trackStyle"]),
        scrollbar_thumb_style: js_style_fn(bridge, &init["thumbStyle"]),
        scrollbar_hide_delay_ms: init["hideDelayMs"].as_f64().map(|v| v.max(0.0) as u64),
        event_loop: None,
    };
    let state = ScrollView::new(child, options);
    let content = init["contentHeight"].as_f64().unwrap_or(0.0) as i64;
    let viewport = init["viewportHeight"].as_f64().unwrap_or(0.0) as i64;
    state.update_layout(content, viewport, Arc::new(|| {}));
    if !init["followingEnd"].as_bool().unwrap_or(false) {
        state.scroll_to(init["scrollTop"].as_f64().unwrap_or(0.0) as i64, ScrollViewScrollToOptions { disable_follow: false });
    }
    if init["scrollbarActive"].as_bool() == Some(true) {
        state.set_scrollbar_active(true);
    }
    state
}

/// `sv` host calls: a JavaScript ScrollView laid out by pi delegates its state to the Rust mirror.
/// `null` when the mirror is gone (the JavaScript ScrollView then uses its own state again).
fn scroll_view_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let Some(sv) = bridge.components.scroll_mirrors.lock().get(&id).and_then(std::sync::Weak::upgrade) else {
        return Ok(Value::Null);
    };
    let a = |i: usize| arg(payload, i);
    let int = |v: &Value| v.as_f64().filter(|v| v.is_finite()).map(|v| v.trunc() as i64);
    let result = match str_arg(payload, "m").as_str() {
        "state" => Value::Null,
        "scrollTo" => {
            let top = int(&a(0)).unwrap_or_else(|| sv.scroll_top());
            sv.scroll_to(top, ScrollViewScrollToOptions { disable_follow: a(1)["disableFollow"].as_bool() == Some(true) });
            Value::Null
        }
        "scrollBy" => json!(sv.scroll_by(int(&a(0)).unwrap_or(0))),
        "scrollToStart" => {
            sv.scroll_to_start();
            Value::Null
        }
        "scrollToEnd" => {
            sv.scroll_to_end();
            Value::Null
        }
        "setScrollbar" => {
            if let Some(scrollbar) = parse_scrollbar(&a(0)) {
                sv.set_scrollbar(scrollbar);
            }
            Value::Null
        }
        "setScrollbarActive" => {
            sv.set_scrollbar_active(a(0).as_bool() == Some(true));
            Value::Null
        }
        "updateLayout" => {
            sv.update_layout(int(&a(0)).unwrap_or(0), int(&a(1)).unwrap_or(0), Arc::new(|| {}));
            Value::Null
        }
        other => return Err(anyhow::anyhow!("ScrollView.{other} is not available")),
    };
    Ok(json!({
        "result": result,
        "state": {
            "scrollTop": sv.scroll_top(),
            "followingEnd": sv.is_following_end(),
            "viewportHeight": sv.viewport_height(),
            "contentHeight": sv.content_height(),
            "scrollbar": scrollbar_name(sv.scrollbar()),
            "scrollbarVisible": sv.is_scrollbar_visible(),
            "scrollbarActive": sv.is_scrollbar_active(),
        },
    }))
}

impl Drop for JsComponent {
    fn drop(&mut self) {
        if self.has_dispose {
            post_call(&self.handle, Some("dispose"), vec![]);
        }
    }
}

/// A JS editor (`setEditorComponent` factory result) as a pi-tui [`EditorComponent`].
pub struct JsEditorComponent {
    inner: JsComponent,
}

impl JsEditorComponent {
    /// The wrapped JS component.
    pub fn js(&self) -> &JsComponent {
        &self.inner
    }

    fn set_callback(&self, property: &str, callback: Option<RustCallbackFn>) {
        let value = match callback {
            Some(cb) => json!(self.inner.bridge.register_callback(cb)),
            None => Value::Null,
        };
        let _ = self
            .inner
            .bridge
            .glue_call_blocking("setPropCallback", vec![self.inner.handle.to_value(), json!(property), value]);
    }

    /// The Rust `CustomEditor` behind a JS `CustomEditor` subclass, if this is one.
    pub fn rust_custom_editor(&self) -> Option<Arc<CustomEditor>> {
        let component = self.inner.rust_component()?;
        pi_tui::tui::downcast_component::<CustomEditor>(&component)
    }

    /// TS `setCustomEditorComponent`: when the JS editor extends `CustomEditor` (has an
    /// `actionHandlers` Map), give it the default editor's escape / ctrl+d / paste-image /
    /// extension-shortcut handlers it does not set itself and copy the app action handlers.
    /// Returns whether the editor is a `CustomEditor`.
    pub fn adopt_app_handlers(&self, default_editor: &Arc<CustomEditor>) -> bool {
        let bridge = &self.inner.bridge;
        // Like TS, the copied handlers call the default editor's handler current at call time.
        let lazy = |get: fn(&CustomEditor) -> Option<Arc<dyn Fn() + Send + Sync>>| {
            let d = default_editor.clone();
            bridge.register_callback(Arc::new(move |_| {
                if let Some(f) = get(&d) {
                    f();
                }
                Ok(Value::Null)
            }))
        };
        let d = default_editor.clone();
        let shortcut = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
            let data = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            Ok(json!(d.on_extension_shortcut().is_some_and(|f| f(data))))
        }));
        let handlers = json!({
            "onEscape": lazy(CustomEditor::on_escape),
            "onCtrlD": lazy(CustomEditor::on_ctrl_d),
            "onPasteImage": lazy(CustomEditor::on_paste_image),
            "onExtensionShortcut": shortcut,
        });
        let void = |f: Arc<dyn Fn() + Send + Sync>| {
            bridge.register_callback(Arc::new(move |_| {
                f();
                Ok(Value::Null)
            }))
        };
        let actions: Vec<Value> = default_editor
            .action_handlers()
            .into_iter()
            .map(|(action, f)| json!([action, void(f)]))
            .collect();
        bridge
            .glue_call_blocking("adoptAppHandlers", vec![self.inner.handle.to_value(), handlers, Value::Array(actions)])
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn call_if_exists(&self, method: &str, args: Vec<Value>) -> Value {
        self.inner
            .bridge
            .glue_call_blocking("callMethodIfExists", vec![self.inner.handle.to_value(), json!(method), json!(args)])
            .unwrap_or(Value::Null)
    }
}

type RustCallbackFn = super::RustCallback;

impl Component for JsEditorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }
    fn handle_input(&self, data: &str) {
        self.inner.handle_input(data)
    }
    fn handles_input(&self) -> bool {
        self.inner.handles_input()
    }
    fn wants_key_release(&self) -> bool {
        self.inner.wants_key_release()
    }
    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.inner.handle_mouse(event)
    }
    fn invalidate(&self) {
        self.inner.invalidate()
    }
    fn is_focusable(&self) -> bool {
        self.inner.is_focusable()
    }
    fn set_focused(&self, focused: bool) {
        self.inner.set_focused(focused)
    }
    fn focused(&self) -> bool {
        self.inner.focused()
    }
}

impl EditorComponent for JsEditorComponent {
    fn get_text(&self) -> String {
        self.call_if_exists("getText", vec![]).as_str().unwrap_or_default().to_string()
    }
    fn set_text(&self, text: &str) {
        self.call_if_exists("setText", vec![json!(text)]);
    }
    fn set_on_submit(&self, f: Option<TextCallback>) {
        self.set_callback(
            "onSubmit",
            f.map(|f| {
                Arc::new(move |args: Vec<Value>| {
                    f(args.first().and_then(Value::as_str).unwrap_or_default().to_string());
                    Ok(Value::Null)
                }) as RustCallbackFn
            }),
        );
    }
    fn set_on_change(&self, f: Option<TextCallback>) {
        self.set_callback(
            "onChange",
            f.map(|f| {
                Arc::new(move |args: Vec<Value>| {
                    f(args.first().and_then(Value::as_str).unwrap_or_default().to_string());
                    Ok(Value::Null)
                }) as RustCallbackFn
            }),
        );
    }
    fn add_to_history(&self, text: &str) {
        self.call_if_exists("addToHistory", vec![json!(text)]);
    }
    fn insert_text_at_cursor(&self, text: &str) {
        self.call_if_exists("insertTextAtCursor", vec![json!(text)]);
    }
    fn get_expanded_text(&self) -> String {
        match self.call_if_exists("getExpandedText", vec![]) {
            Value::String(s) => s,
            _ => self.get_text(),
        }
    }
    fn set_autocomplete_provider(&self, provider: Arc<dyn AutocompleteProvider>) {
        let id = self.inner.bridge.components.autocomplete.insert(provider);
        let _ = self.inner.bridge.glue_call_blocking(
            "callMethodIfExists",
            vec![self.inner.handle.to_value(), json!("setAutocompleteProvider"), json!([{ "$piAutocomplete": id }])],
        );
    }
    fn set_border_color(&self, f: StyleFn) {
        self.set_callback(
            "borderColor",
            Some(Arc::new(move |args: Vec<Value>| Ok(json!(f(args.first().and_then(Value::as_str).unwrap_or_default()))))),
        );
    }
    fn set_padding_x(&self, padding: usize) {
        self.call_if_exists("setPaddingX", vec![json!(padding)]);
    }
    fn set_autocomplete_max_visible(&self, max_visible: usize) {
        self.call_if_exists("setAutocompleteMaxVisible", vec![json!(max_visible)]);
    }
}

/// Placeholder for renderers that produced nothing.
struct EmptyComponent;

impl Component for EmptyComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        Vec::new()
    }
}

/// Resolve a component description returned by bridge.js (`componentRef`).
pub(crate) fn component_from_value(bridge: &Arc<Bridge>, value: &Value) -> Option<ComponentRef> {
    if value.is_null() {
        return None;
    }
    if let Some(id) = value.get("$piComponent").and_then(Value::as_u64) {
        return bridge.components.rust.get(id).map(|c| c.component());
    }
    let handle = bridge.handle(&value["$piJsComponent"])?;
    Some(Arc::new(JsComponent::new(bridge.clone(), handle, value)))
}

/// TS `TuiMouseEvent` for a pi-tui mouse event.
fn mouse_event_json(event: &MouseEvent) -> Value {
    let kind = match event.kind {
        MouseEventType::Press => "press",
        MouseEventType::Release => "release",
        MouseEventType::Move => "move",
        MouseEventType::Drag => "drag",
        MouseEventType::Click => "click",
        MouseEventType::Wheel => "wheel",
    };
    let button = match event.button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
        MouseButton::None => "none",
    };
    let mut out = json!({
        "type": kind, "button": button, "x": event.x, "y": event.y, "screenX": event.screen_x,
        "screenY": event.screen_y, "width": event.width, "height": event.height,
        "shift": event.shift, "alt": event.alt, "ctrl": event.ctrl,
    });
    if let Some(delta) = event.wheel_delta {
        out["wheelDelta"] = json!(delta);
    }
    if let Some(count) = event.click_count {
        out["clickCount"] = json!(count);
    }
    out
}

fn mouse_event_from(value: &Value) -> MouseEvent {
    let i = |k: &str| value[k].as_f64().unwrap_or(0.0) as i64;
    let b = |k: &str| value[k].as_bool().unwrap_or(false);
    MouseEvent {
        kind: match value["type"].as_str() {
            Some("release") => MouseEventType::Release,
            Some("move") => MouseEventType::Move,
            Some("drag") => MouseEventType::Drag,
            Some("click") => MouseEventType::Click,
            Some("wheel") => MouseEventType::Wheel,
            _ => MouseEventType::Press,
        },
        button: match value["button"].as_str() {
            Some("left") => MouseButton::Left,
            Some("middle") => MouseButton::Middle,
            Some("right") => MouseButton::Right,
            _ => MouseButton::None,
        },
        x: i("x"),
        y: i("y"),
        screen_x: i("screenX"),
        screen_y: i("screenY"),
        width: i("width"),
        height: i("height"),
        shift: b("shift"),
        alt: b("alt"),
        ctrl: b("ctrl"),
        wheel_delta: value["wheelDelta"].as_f64().map(|v| v as i64),
        click_count: value["clickCount"].as_f64().map(|v| v as u32),
    }
}

/// A JS `handleMouse()` result (`invokeHandleMouse` in bridge.js): flags, plus the dispatch
/// target and focus target when a JS container forwarded the event to a child.
fn mouse_result_from(bridge: &Arc<Bridge>, value: &Value) -> Option<MouseResult> {
    if !value.is_object() {
        return None;
    }
    let target = value.get("target").filter(|t| t.is_object()).and_then(|t| {
        Some(MouseDispatchTarget {
            component: component_from_value(bridge, &t["component"])?,
            origin_x: t["originX"].as_f64().unwrap_or(0.0) as i64,
            origin_y: t["originY"].as_f64().unwrap_or(0.0) as i64,
            width: t["width"].as_f64().unwrap_or(0.0) as i64,
            height: t["height"].as_f64().unwrap_or(0.0) as i64,
        })
    });
    Some(MouseResult {
        handled: value["handled"].as_bool().unwrap_or(false),
        capture: value["capture"].as_bool().unwrap_or(false),
        focus: value["focus"].as_bool().unwrap_or(false),
        render: value["render"].as_bool(),
        target,
        focus_target: value.get("focusTarget").and_then(|f| component_from_value(bridge, f)),
    })
}

/// A markdown theme reference from JS (`markdownThemeRef` in pi-tui.js): `None` when absent.
pub(crate) fn markdown_theme_ref(bridge: &Bridge, value: &Value) -> Option<MarkdownTheme> {
    match value {
        Value::Null => None,
        t if t["builtin"].is_string() => Some(crate::modes::interactive::theme::theme::get_markdown_theme()),
        t => Some(t["id"].as_u64().and_then(|id| bridge.components.markdown_themes.get(id)).unwrap_or_else(MarkdownTheme::plain)),
    }
}

fn render_error(err: anyhow::Error) -> ComponentRef {
    let (message, _) = crate::core::extensions::runner::error_parts(&err);
    Arc::new(RenderFailed { message })
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

/// `setWidget` / `setHeader` factory `(tui, theme) => component`.
pub(crate) fn js_component_factory(bridge: Arc<Bridge>, factory: JsHandle) -> ComponentFactory {
    Arc::new(move |tui: Arc<Tui>, theme: Arc<Theme>| {
        let args = vec![factory.to_value(), json!(tui_id(&bridge, &tui)), json!(theme_id(&bridge, &theme)), Value::Null, Value::Null];
        match bridge.glue_call_blocking("makeComponentSync", args) {
            Ok(v) => component_from_value(&bridge, &v).unwrap_or_else(|| Arc::new(EmptyComponent)),
            Err(e) => render_error(e),
        }
    })
}

/// `setFooter` factory `(tui, theme, footerData) => component`.
pub(crate) fn js_footer_factory(bridge: Arc<Bridge>, factory: JsHandle) -> FooterFactory {
    Arc::new(move |tui: Arc<Tui>, theme: Arc<Theme>, footer: ReadonlyFooterDataProvider| {
        let footer_id = bridge.components.footers.insert(footer);
        let args = vec![
            factory.to_value(),
            json!(tui_id(&bridge, &tui)),
            json!(theme_id(&bridge, &theme)),
            json!(footer_id),
            json!("footer"),
        ];
        match bridge.glue_call_blocking("makeComponentSync", args) {
            Ok(v) => component_from_value(&bridge, &v).unwrap_or_else(|| Arc::new(EmptyComponent)),
            Err(e) => render_error(e),
        }
    })
}

fn style_callback(bridge: &Bridge, f: &StyleFn) -> u64 {
    let f = f.clone();
    bridge.register_callback(Arc::new(move |args: Vec<Value>| Ok(json!(f(args.first().and_then(Value::as_str).unwrap_or_default())))))
}

fn editor_theme_json(bridge: &Bridge, theme: &EditorTheme) -> Value {
    json!({
        "borderColor": style_callback(bridge, &theme.border_color),
        "selectList": {
            "selectedPrefix": style_callback(bridge, &theme.select_list.selected_prefix),
            "selectedText": style_callback(bridge, &theme.select_list.selected_text),
            "description": style_callback(bridge, &theme.select_list.description),
            "scrollInfo": style_callback(bridge, &theme.select_list.scroll_info),
            "noMatch": style_callback(bridge, &theme.select_list.no_match),
        }
    })
}

/// `setEditorComponent` factory `(tui, editorTheme, keybindings) => editor`.
pub(crate) fn js_editor_factory(bridge: Arc<Bridge>, factory: JsHandle) -> EditorFactory {
    Arc::new(move |tui: Arc<Tui>, theme: EditorTheme, keybindings: Arc<KeybindingsManager>| {
        let args = vec![
            factory.to_value(),
            json!(tui_id(&bridge, &tui)),
            editor_theme_json(&bridge, &theme),
            json!(keybindings_id(&bridge, &keybindings)),
        ];
        let value = bridge.glue_call_blocking("makeEditorComponent", args).unwrap_or(Value::Null);
        let inner = match bridge.handle(&value["$piJsComponent"]) {
            Some(handle) => JsComponent::new(bridge.clone(), handle, &value),
            None => {
                let handle = bridge.rt.handle(&json!({ "$jsHandle": 0 }));
                match handle {
                    Some(h) => JsComponent::new(bridge.clone(), h, &Value::Null),
                    None => unreachable!("handle markers always adopt"),
                }
            }
        };
        Arc::new(JsEditorComponent { inner }) as Arc<dyn EditorComponent>
    })
}

/// `ui.custom()` factory `(tui, theme, keybindings, done) => component | Promise<component>`.
pub(crate) fn js_custom_factory(bridge: Arc<Bridge>, factory: JsHandle) -> CustomComponentFactory {
    Arc::new(move |tui: Arc<Tui>, theme: Arc<Theme>, keybindings: Arc<KeybindingsManager>, done: CustomUiDone| {
        let bridge = bridge.clone();
        let factory = factory.clone();
        Box::pin(async move {
            let done_id = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
                done(Box::new(JsUiValue(args.into_iter().next().unwrap_or(Value::Null))));
                Ok(Value::Null)
            }));
            let args = vec![
                factory.to_value(),
                json!(tui_id(&bridge, &tui)),
                json!(theme_id(&bridge, &theme)),
                json!(keybindings_id(&bridge, &keybindings)),
                json!(done_id),
            ];
            let value = bridge.glue_call("makeComponent", args).await?;
            component_from_value(&bridge, &value).ok_or_else(|| anyhow::anyhow!("ui.custom() factory returned no component"))
        }) as BoxFuture<'static, anyhow::Result<ComponentRef>>
    })
}

fn size_value(value: &Value) -> Option<SizeValue> {
    match value {
        Value::Number(n) => n.as_f64().map(|v| SizeValue::Abs(v as i64)),
        Value::String(s) => Some(SizeValue::Percent(s.clone())),
        _ => None,
    }
}

fn overlay_options(bridge: &Arc<Bridge>, value: &Value) -> OverlayOptions {
    let anchor = value["anchor"].as_str().map(|a| match a {
        "top-left" => OverlayAnchor::TopLeft,
        "top-right" => OverlayAnchor::TopRight,
        "bottom-left" => OverlayAnchor::BottomLeft,
        "bottom-right" => OverlayAnchor::BottomRight,
        "top-center" => OverlayAnchor::TopCenter,
        "bottom-center" => OverlayAnchor::BottomCenter,
        "left-center" => OverlayAnchor::LeftCenter,
        "right-center" => OverlayAnchor::RightCenter,
        _ => OverlayAnchor::Center,
    });
    let margin = match &value["margin"] {
        Value::Number(n) => Some(OverlayMargin::all(n.as_f64().unwrap_or(0.0) as i64)),
        Value::Object(m) => Some(OverlayMargin {
            top: m.get("top").and_then(Value::as_f64).map(|v| v as i64),
            right: m.get("right").and_then(Value::as_f64).map(|v| v as i64),
            bottom: m.get("bottom").and_then(Value::as_f64).map(|v| v as i64),
            left: m.get("left").and_then(Value::as_f64).map(|v| v as i64),
        }),
        _ => None,
    };
    let visible = bridge.handle(&value["visible"]).map(|h| {
        Arc::new(move |w: usize, rows: usize| h.call_blocking(vec![json!(w), json!(rows)]).ok().and_then(|v| v.as_bool()).unwrap_or(true))
            as pi_tui::tui::OverlayVisibleFn
    });
    OverlayOptions {
        width: size_value(&value["width"]),
        min_width: value["minWidth"].as_f64().map(|v| v as i64),
        max_height: size_value(&value["maxHeight"]),
        anchor,
        offset_x: value["offsetX"].as_f64().map(|v| v as i64),
        offset_y: value["offsetY"].as_f64().map(|v| v as i64),
        row: size_value(&value["row"]),
        col: size_value(&value["col"]),
        margin,
        visible,
        non_capturing: value["nonCapturing"].as_bool().unwrap_or(false),
    }
}

/// Options of `ui.custom()`.
pub(crate) fn custom_ui_options(bridge: &Arc<Bridge>, value: &Value) -> CustomUiOptions {
    let overlay_options = match &value["overlayOptions"] {
        Value::Null => None,
        v => match bridge.handle(v) {
            Some(h) => {
                let b = bridge.clone();
                Some(OverlayOptionsSource::Dynamic(Arc::new(move || {
                    let value = h.call_blocking(vec![]).unwrap_or(Value::Null);
                    overlay_options(&b, &value)
                })))
            }
            None => Some(OverlayOptionsSource::Static(overlay_options(bridge, v))),
        },
    };
    let on_handle = bridge.handle(&value["onHandle"]).map(|h| {
        let b = bridge.clone();
        Arc::new(move |handle: OverlayHandle| {
            let id = b.components.overlays.insert(handle);
            post_call(&b.glue, Some("invokeWithOverlay"), vec![h.to_value(), json!(id)]);
        }) as Arc<dyn Fn(OverlayHandle) + Send + Sync>
    });
    CustomUiOptions { overlay: value["overlay"].as_bool(), overlay_options, on_handle }
}

/// `addAutocompleteProvider` factory `(current) => provider`.
pub(crate) fn js_autocomplete_factory(bridge: Arc<Bridge>, factory: JsHandle) -> AutocompleteProviderFactory {
    Arc::new(move |current: Arc<dyn AutocompleteProvider>| {
        let id = bridge.components.autocomplete.insert(current.clone());
        match bridge.glue_call_blocking("makeAutocompleteProvider", vec![factory.to_value(), json!(id)]) {
            Ok(v) => match bridge.handle(&v) {
                Some(handle) => {
                    // TS: `provider.shouldTriggerFileCompletion?.(...)`; only an implemented method gates
                    // forced (Tab) file completion.
                    let has_should_trigger = bridge
                        .glue_call_blocking("hasMethod", vec![handle.to_value(), json!("shouldTriggerFileCompletion")])
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    Arc::new(JsAutocompleteProvider { bridge: bridge.clone(), handle, has_should_trigger }) as Arc<dyn AutocompleteProvider>
                }
                None => current,
            },
            Err(_) => current,
        }
    })
}

/// A JS autocomplete provider.
struct JsAutocompleteProvider {
    bridge: Arc<Bridge>,
    handle: JsHandle,
    has_should_trigger: bool,
}

fn completion_result(value: &Value, lines: &[String], cursor_line: usize, cursor_col: usize) -> CompletionResult {
    CompletionResult {
        lines: opt::<Vec<String>>(&value["lines"]).ok().flatten().unwrap_or_else(|| lines.to_vec()),
        cursor_line: value["cursorLine"].as_u64().map(|v| v as usize).unwrap_or(cursor_line),
        cursor_col: value["cursorCol"].as_u64().map(|v| v as usize).unwrap_or(cursor_col),
    }
}

fn suggestions_from(value: &Value) -> Option<AutocompleteSuggestions> {
    if value.is_null() {
        return None;
    }
    Some(AutocompleteSuggestions {
        items: value["items"].as_array().map(|a| a.iter().map(autocomplete_item).collect()).unwrap_or_default(),
        prefix: value["prefix"].as_str().unwrap_or_default().to_string(),
    })
}

fn suggestions_json(s: &Option<AutocompleteSuggestions>) -> Value {
    match s {
        Some(s) => json!({ "items": s.items.iter().map(autocomplete_item_json).collect::<Vec<_>>(), "prefix": s.prefix }),
        None => Value::Null,
    }
}

impl AutocompleteProvider for JsAutocompleteProvider {
    fn trigger_characters(&self) -> Vec<String> {
        self.handle
            .get_blocking("triggerCharacters")
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: SuggestOptions,
    ) -> pi_tui::event_loop::BoxFuture<Option<AutocompleteSuggestions>> {
        let args = json!([lines, cursor_line, cursor_col, { "force": options.force }]);
        let bridge = self.bridge.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            let value = bridge.glue_call("callMethodAsync", vec![handle.to_value(), json!("getSuggestions"), args]).await.ok()?;
            suggestions_from(&value)
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let args = json!([lines, cursor_line, cursor_col, autocomplete_item_json(item), prefix]);
        match self.bridge.glue_call_blocking("callMethod", vec![self.handle.to_value(), json!("applyCompletion"), args]) {
            Ok(v) => completion_result(&v, lines, cursor_line, cursor_col),
            Err(_) => CompletionResult { lines: lines.to_vec(), cursor_line, cursor_col },
        }
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        if !self.has_should_trigger {
            return true;
        }
        let args = json!([lines, cursor_line, cursor_col]);
        // Like TS, a throwing provider propagates no completion (the request is dropped).
        self.bridge
            .glue_call_blocking("callMethodTruthy", vec![self.handle.to_value(), json!("shouldTriggerFileCompletion"), args])
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn has_should_trigger_file_completion(&self) -> bool {
        self.has_should_trigger
    }
}

use crate::compat::AutocompleteItem;

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

fn render_context_json(bridge: &Arc<Bridge>, ctx: &ToolRenderContext) -> Value {
    let invalidate = ctx.invalidate.clone();
    let invalidate_id = bridge.register_callback_keyed(Arc::as_ptr(&ctx.invalidate) as *const () as usize, || {
        Arc::new(move |_| {
            invalidate();
            Ok(Value::Null)
        })
    });
    let state = {
        let existing = ctx.state.lock().as_ref().and_then(|s| s.downcast_ref::<JsRenderState>().map(|s| s.0.clone()));
        match existing {
            Some(h) => Some(h),
            None => {
                let created = bridge.glue_call_blocking("newState", vec![]).ok().and_then(|v| bridge.handle(&v));
                if let Some(h) = &created {
                    let mut slot = ctx.state.lock();
                    if slot.is_none() {
                        *slot = Some(Box::new(JsRenderState(h.clone())));
                    }
                }
                created
            }
        }
    };
    let last = ctx
        .last_component
        .as_ref()
        .and_then(pi_tui::tui::downcast_component::<JsComponent>)
        .map(|c| c.handle.to_value())
        .unwrap_or(Value::Null);
    let rid = {
        let mut ids = bridge.components.render_context_ids.lock();
        let id = *ids.entry(ctx.tool_call_id.clone()).or_insert_with(super::next_id);
        bridge.components.render_contexts.insert_with_id(id, ctx.clone());
        id
    };
    let mut out = json!({
        "args": ctx.args,
        "toolCallId": ctx.tool_call_id,
        "cwd": ctx.cwd,
        "executionStarted": ctx.execution_started,
        "argsComplete": ctx.args_complete,
        "isPartial": ctx.is_partial,
        "expanded": ctx.expanded,
        "showImages": ctx.show_images,
        "isError": ctx.is_error,
        "outputPad": ctx.output_pad,
        "invalidate": invalidate_id,
        "lastComponent": last,
        "state": state.map(|h| h.to_value()).unwrap_or(Value::Null),
        "$rid": rid,
    });
    if let Some(d) = ctx.duration_ms {
        out["durationMs"] = json!(d);
    }
    out
}

fn js_render_call(bridge: Arc<Bridge>, handle: JsHandle) -> ToolRenderCallFn {
    Arc::new(move |args: &Value, theme: &Arc<Theme>, ctx: &ToolRenderContext| {
        let call_args = vec![handle.to_value(), args.clone(), json!(theme_id(&bridge, theme)), render_context_json(&bridge, ctx)];
        match bridge.glue_call_blocking("renderCall", call_args) {
            Ok(v) => component_from_value(&bridge, &v).unwrap_or_else(|| Arc::new(EmptyComponent)),
            Err(e) => render_error(e),
        }
    })
}

fn js_render_result(bridge: Arc<Bridge>, handle: JsHandle) -> ToolRenderResultFn {
    Arc::new(move |result: &pi_agent::types::AgentToolResult, options: ToolRenderResultOptions, theme: &Arc<Theme>, ctx: &ToolRenderContext| {
        let call_args = vec![
            handle.to_value(),
            serde_json::to_value(result).unwrap_or(Value::Null),
            serde_json::to_value(options).unwrap_or(Value::Null),
            json!(theme_id(&bridge, theme)),
            render_context_json(&bridge, ctx),
        ];
        match bridge.glue_call_blocking("renderResult", call_args) {
            Ok(v) => component_from_value(&bridge, &v).unwrap_or_else(|| Arc::new(EmptyComponent)),
            Err(e) => render_error(e),
        }
    })
}

/// `renderCall` / `renderResult` of a JS tool descriptor: JS functions or Rust markers.
pub(crate) fn tool_renderers(
    bridge: &Arc<Bridge>,
    render_call: &Value,
    render_result: &Value,
) -> anyhow::Result<(Option<ToolRenderCallFn>, Option<ToolRenderResultFn>)> {
    let call = match rust_marker(render_call) {
        Some(marker) => rust_renderers(bridge, marker).render_call,
        None => bridge.handle(render_call).map(|h| js_render_call(bridge.clone(), h)),
    };
    let result = match rust_marker(render_result) {
        Some(marker) => rust_renderers(bridge, marker).render_result,
        None => bridge.handle(render_result).map(|h| js_render_result(bridge.clone(), h)),
    };
    Ok((call, result))
}

/// Renderers implemented in Rust named by a `$piRust` marker.
fn rust_renderers(bridge: &Bridge, marker: &Value) -> RendererEntry {
    if let Some(id) = marker["renderer"].as_u64() {
        return bridge.components.renderers.get(id).unwrap_or(RendererEntry { render_call: None, render_result: None });
    }
    if let Some(id) = marker["tool"].as_u64()
        && let Some(tool) = bridge.builtins.get(id)
    {
        return RendererEntry { render_call: tool.definition.render_call.clone(), render_result: tool.definition.render_result.clone() };
    }
    RendererEntry { render_call: None, render_result: None }
}

pub(crate) fn js_message_renderer(bridge: Arc<Bridge>, handle: JsHandle) -> MessageRenderer {
    Arc::new(move |message: &crate::core::messages::CustomMessage, options: &MessageRenderOptions, theme: &Arc<Theme>| {
        let args = vec![
            handle.to_value(),
            serde_json::to_value(message).unwrap_or(Value::Null),
            serde_json::to_value(options).unwrap_or(Value::Null),
            json!(theme_id(&bridge, theme)),
        ];
        match bridge.glue_call_blocking("renderMessage", args) {
            Ok(v) => component_from_value(&bridge, &v),
            Err(e) => Some(render_error(e)),
        }
    })
}

pub(crate) fn js_entry_renderer(bridge: Arc<Bridge>, handle: JsHandle) -> EntryRenderer {
    Arc::new(move |entry: &crate::core::session_manager::CustomEntry, options: &EntryRenderOptions, theme: &Arc<Theme>| {
        let args = vec![
            handle.to_value(),
            custom_entry_json(entry),
            serde_json::to_value(options).unwrap_or(Value::Null),
            json!(theme_id(&bridge, theme)),
        ];
        match bridge.glue_call_blocking("renderMessage", args) {
            Ok(v) => component_from_value(&bridge, &v),
            Err(e) => Some(render_error(e)),
        }
    })
}

/// A JS tool renderer resolver `(toolName, next) => renderers | undefined`. `next()` is
/// evaluated before calling JS (renderers are pure), and offered to JS as Rust renderers.
pub(crate) fn js_tool_renderer_resolver(bridge: Arc<Bridge>, resolver: JsHandle) -> ToolRendererResolver {
    Arc::new(move |tool_name: &str, next: &dyn Fn() -> Option<ToolRenderers>| {
        let next_value = match next() {
            Some(renderers) => {
                let entry = RendererEntry { render_call: renderers.render_call.clone(), render_result: renderers.render_result.clone() };
                let call_id = renderers.render_call.as_ref().map(|_| bridge.components.renderers.insert(entry.clone()));
                let result_id = renderers.render_result.as_ref().map(|_| bridge.components.renderers.insert(entry.clone()));
                json!({
                    "renderShell": serde_json::to_value(renderers.render_shell).unwrap_or(Value::Null),
                    "renderCall": call_id,
                    "renderResult": result_id,
                })
            }
            None => Value::Null,
        };
        let value = bridge
            .glue_call_blocking("resolveToolRenderers", vec![resolver.to_value(), json!(tool_name), next_value])
            .ok()?;
        if value.is_null() {
            return None;
        }
        let (render_call, render_result) = tool_renderers(&bridge, &value["renderCall"], &value["renderResult"]).ok()?;
        Some(ToolRenderers { render_shell: opt(&value["renderShell"]).ok().flatten(), render_call, render_result })
    })
}

// ---------------------------------------------------------------------------
// Host calls
// ---------------------------------------------------------------------------

fn js_style_fn(bridge: &Arc<Bridge>, value: &Value) -> Option<StyleFn> {
    let handle = bridge.handle(value)?;
    Some(Arc::new(move |text: &str| match handle.call_blocking(vec![json!(text)]) {
        Ok(Value::String(s)) => s,
        _ => text.to_string(),
    }))
}

fn identity_style() -> StyleFn {
    Arc::new(|t: &str| t.to_string())
}

fn select_list_theme(bridge: &Arc<Bridge>, value: &Value) -> SelectListTheme {
    SelectListTheme {
        selected_prefix: js_style_fn(bridge, &value["selectedPrefix"]).unwrap_or_else(identity_style),
        selected_text: js_style_fn(bridge, &value["selectedText"]).unwrap_or_else(identity_style),
        description: js_style_fn(bridge, &value["description"]).unwrap_or_else(identity_style),
        scroll_info: js_style_fn(bridge, &value["scrollInfo"]).unwrap_or_else(identity_style),
        no_match: js_style_fn(bridge, &value["noMatch"]).unwrap_or_else(identity_style),
    }
}

fn markdown_theme(bridge: &Arc<Bridge>, value: &Value) -> MarkdownTheme {
    let style = |key: &str| js_style_fn(bridge, &value[key]).unwrap_or_else(identity_style);
    let mut theme = MarkdownTheme::plain();
    theme.heading = style("heading");
    theme.link = style("link");
    theme.link_url = style("linkUrl");
    theme.code = style("code");
    theme.code_block = style("codeBlock");
    theme.code_block_border = style("codeBlockBorder");
    theme.quote = style("quote");
    theme.quote_border = style("quoteBorder");
    theme.hr = style("hr");
    theme.list_bullet = style("listBullet");
    theme.bold = style("bold");
    theme.italic = style("italic");
    theme.strikethrough = style("strikethrough");
    theme.underline = style("underline");
    if let Some(h) = bridge.handle(&value["highlightCode"]) {
        theme.highlight_code = Some(Arc::new(move |code: &str, lang: Option<&str>| {
            h.call_blocking(vec![json!(code), json!(lang)])
                .ok()
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_else(|| code.split('\n').map(str::to_string).collect())
        }));
    }
    if let Some(indent) = value["codeBlockIndent"].as_str() {
        theme.code_block_indent = Some(indent.to_string());
    }
    theme
}

fn default_text_style(bridge: &Arc<Bridge>, value: &Value) -> Option<DefaultTextStyle> {
    if !value.is_object() {
        return None;
    }
    Some(DefaultTextStyle {
        color: js_style_fn(bridge, &value["color"]),
        bg_color: js_style_fn(bridge, &value["bgColor"]),
        bold: value["bold"].as_bool().unwrap_or(false),
        italic: value["italic"].as_bool().unwrap_or(false),
        strikethrough: value["strikethrough"].as_bool().unwrap_or(false),
        underline: value["underline"].as_bool().unwrap_or(false),
    })
}

fn lines_json(lines: Vec<String>) -> Value {
    Value::Array(lines.into_iter().map(Value::String).collect())
}

fn utf16_to_byte(text: &str, index: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= index {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

pub(crate) fn dispatch(bridge: &Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "theme" => theme_call(bridge, &payload),
        "tui" => tui_call(bridge, &payload),
        "kb" => keybindings_call(bridge, &payload),
        "sv" => scroll_view_call(bridge, &payload),
        "overlay" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let handle = bridge.components.overlays.get(id).ok_or_else(|| anyhow::anyhow!("unknown overlay"))?;
            let method = str_arg(&payload, "m");
            Ok(match method.as_str() {
                "hide" => {
                    bridge.ui_dispatch(move || handle.hide());
                    Value::Null
                }
                "setHidden" => {
                    let hidden = arg(&payload, 0).as_bool().unwrap_or(false);
                    bridge.ui_dispatch(move || handle.set_hidden(hidden));
                    Value::Null
                }
                "isHidden" => json!(handle.is_hidden()),
                "focus" => {
                    bridge.ui_dispatch(move || handle.focus());
                    Value::Null
                }
                "unfocus" => {
                    bridge.ui_dispatch(move || handle.unfocus(None));
                    Value::Null
                }
                "isFocused" => json!(bridge.ui_dispatch_wait(move || handle.is_focused())?),
                _ => Value::Null,
            })
        }
        "footer" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let footer = bridge.components.footers.get(id).ok_or_else(|| anyhow::anyhow!("unknown footer data"))?;
            Ok(match str_arg(&payload, "m").as_str() {
                "getGitBranch" => json!(footer.get_git_branch()),
                "getExtensionStatuses" => serde_json::to_value(footer.get_extension_statuses())?,
                "getAvailableProviderCount" => json!(footer.get_available_provider_count()),
                "onBranchChange" => {
                    let handle = bridge.handle(&arg(&payload, 0)).ok_or_else(|| anyhow::anyhow!("onBranchChange expects a callback"))?;
                    let unsubscribe = footer.on_branch_change(Arc::new(move || post_call(&handle, None, vec![])));
                    json!(bridge.unsubscribes.insert(unsubscribe))
                }
                _ => Value::Null,
            })
        }
        "ac" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let provider = bridge.components.autocomplete.get(id).ok_or_else(|| anyhow::anyhow!("unknown autocomplete provider"))?;
            let a = |i: usize| arg(&payload, i);
            let lines: Vec<String> = opt(&a(0))?.unwrap_or_default();
            let line = a(1).as_u64().unwrap_or(0) as usize;
            let col = a(2).as_u64().unwrap_or(0) as usize;
            Ok(match str_arg(&payload, "m").as_str() {
                "applyCompletion" => {
                    let item = autocomplete_item(&a(3));
                    let r = provider.apply_completion(&lines, line, col, &item, a(4).as_str().unwrap_or_default());
                    json!({ "lines": r.lines, "cursorLine": r.cursor_line, "cursorCol": r.cursor_col })
                }
                "shouldTriggerFileCompletion" => json!(provider.should_trigger_file_completion(&lines, line, col)),
                "triggerCharacters" => json!(provider.trigger_characters()),
                _ => Value::Null,
            })
        }
        "rc.new" => rust_component_new(bridge, &payload),
        "rc" => rust_component_call(bridge, &payload),
        "tui.util" => tui_util(bridge, &payload),
        "tui.markdown" => {
            let theme = markdown_theme_ref(bridge, &payload["theme"]).unwrap_or_else(MarkdownTheme::plain);
            let markdown = Markdown::new(
                payload["text"].as_str().unwrap_or_default(),
                payload["paddingX"].as_u64().unwrap_or(0) as usize,
                payload["paddingY"].as_u64().unwrap_or(0) as usize,
                theme,
                default_text_style(bridge, &payload["defaultTextStyle"]),
                None,
            );
            Ok(lines_json(markdown.render(payload["width"].as_u64().unwrap_or(80) as usize)))
        }
        "tui.markdownTheme" => Ok(json!(bridge.components.markdown_themes.insert(markdown_theme(bridge, &payload["theme"])))),
        "renderer.call" | "renderer.result" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let entry = bridge.components.renderers.get(id).ok_or_else(|| anyhow::anyhow!("unknown renderer"))?;
            let theme = payload["theme"]
                .as_u64()
                .and_then(|id| bridge.components.themes.get(id))
                .unwrap_or_else(crate::modes::interactive::theme::theme::theme);
            let ctx = payload["context"]
                .as_u64()
                .and_then(|id| bridge.components.render_contexts.get(id))
                .ok_or_else(|| anyhow::anyhow!("built-in renderers need the render context they were called with"))?;
            let component = if op == "renderer.call" {
                let f = entry.render_call.ok_or_else(|| anyhow::anyhow!("no renderCall"))?;
                f(&payload["args"], &theme, &ctx)
            } else {
                let f = entry.render_result.ok_or_else(|| anyhow::anyhow!("no renderResult"))?;
                let result: pi_agent::types::AgentToolResult = serde_json::from_value(payload["result"].clone()).unwrap_or_default();
                let options: ToolRenderResultOptions = opt(&payload["options"])?.unwrap_or_default();
                f(&result, options, &theme, &ctx)
            };
            Ok(json!(register_rust_component(bridge, component)))
        }
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, _op: &str, payload: Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let provider = bridge.components.autocomplete.get(id).ok_or_else(|| anyhow::anyhow!("unknown autocomplete provider"))?;
    let a = |i: usize| arg(&payload, i);
    let lines: Vec<String> = opt(&a(0))?.unwrap_or_default();
    let options = SuggestOptions { force: a(3)["force"].as_bool().unwrap_or(false), ..Default::default() };
    let suggestions =
        provider.get_suggestions(&lines, a(1).as_u64().unwrap_or(0) as usize, a(2).as_u64().unwrap_or(0) as usize, options).await;
    Ok(suggestions_json(&suggestions))
}

/// Expose a Rust component to JS; returns its id (`RustComponent` in bridge.js).
pub(crate) fn register_rust_component(bridge: &Bridge, component: ComponentRef) -> u64 {
    if let Some(js) = pi_tui::tui::downcast_component::<JsComponent>(&component)
        && let Some(id) = js.rust_id
    {
        return id;
    }
    bridge.components.rust.insert(RustComp::Other(component))
}

fn theme_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let theme = bridge.components.themes.get(id).ok_or_else(|| anyhow::anyhow!("unknown theme"))?;
    let method = str_arg(payload, "m");
    let a = |i: usize| arg(payload, i);
    let text = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    Ok(match method.as_str() {
        "fg" => json!(theme.fg(&serde_json::from_value(a(0))?, &text(1))),
        "bg" => json!(theme.bg(&serde_json::from_value(a(0))?, &text(1))),
        "bold" => json!(theme.bold(&text(0))),
        "italic" => json!(theme.italic(&text(0))),
        "underline" => json!(theme.underline(&text(0))),
        "inverse" => json!(theme.inverse(&text(0))),
        "strikethrough" => json!(theme.strikethrough(&text(0))),
        "style" => {
            let o = a(1);
            let style = crate::modes::interactive::theme::theme::ThemeStyle {
                bold: o["bold"].as_bool(),
                dim: o["dim"].as_bool(),
                italic: o["italic"].as_bool(),
                underline: o["underline"].as_bool(),
                inverse: o["inverse"].as_bool(),
                strikethrough: o["strikethrough"].as_bool(),
                fg: serde_json::from_value(o["fg"].clone())
                    .ok()
                    .map(crate::modes::interactive::theme::theme::ThemeStyleFg::ThemeColor),
                bg: serde_json::from_value(o["bg"].clone())
                    .ok()
                    .map(crate::modes::interactive::theme::theme::ThemeStyleBg::ThemeBg),
            };
            json!(theme.style(&text(0), &style))
        }
        "getFgAnsi" => json!(theme.get_fg_ansi(&serde_json::from_value(a(0))?)),
        "getBgAnsi" => json!(theme.get_bg_ansi(&serde_json::from_value(a(0))?)),
        "getColorMode" => json!(theme.get_color_mode().as_str()),
        "thinkingBorder" => {
            let f = theme.get_thinking_border_color(serde_json::from_value(a(0))?);
            json!(f(text(1)))
        }
        "bashBorder" => json!((theme.get_bash_mode_border_color())(text(0))),
        "name" => json!(theme.name()),
        "sourcePath" => json!(theme.source_path()),
        "appearance" => serde_json::to_value(theme.appearance())?,
        _ => return Err(anyhow::anyhow!("theme.{method} is not available")),
    })
}

fn tui_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let tui = bridge.components.tuis.get(id).ok_or_else(|| anyhow::anyhow!("unknown TUI"))?;
    let method = str_arg(payload, "m");
    Ok(match method.as_str() {
        "requestRender" => {
            let force = arg(payload, 0).as_bool().unwrap_or(false);
            bridge.ui_dispatch(move || tui.request_render(force));
            Value::Null
        }
        "invalidate" => {
            bridge.ui_dispatch(move || tui.invalidate());
            Value::Null
        }
        "isViewport" => json!(tui.is_viewport_tui()),
        "columns" => json!(tui.terminal().columns()),
        "rows" => json!(tui.terminal().rows()),
        "write" => {
            let data = arg(payload, 0).as_str().unwrap_or_default().to_string();
            bridge.ui_dispatch(move || tui.terminal().write(&data));
            Value::Null
        }
        "setFocus" => {
            let component = component_from_value(bridge, &arg(payload, 0));
            bridge.ui_dispatch(move || tui.set_focus(component));
            Value::Null
        }
        "showOverlay" => {
            let component = component_from_value(bridge, &arg(payload, 0)).ok_or_else(|| anyhow::anyhow!("showOverlay() expects a component"))?;
            let options = match arg(payload, 1) {
                Value::Null => None,
                v => Some(overlay_options(bridge, &v)),
            };
            let handle = bridge.ui_dispatch_wait(move || tui.show_overlay(component, options))?;
            json!(bridge.components.overlays.insert(handle))
        }
        "hideOverlay" => {
            bridge.ui_dispatch(move || tui.hide_overlay());
            Value::Null
        }
        "hasOverlay" => json!(bridge.ui_dispatch_wait(move || tui.has_overlay())?),
        "start" => {
            bridge.ui_dispatch(move || tui.start());
            Value::Null
        }
        "stop" => {
            bridge.ui_dispatch(move || tui.stop(pi_tui::tui::TuiStopOptions { preserve_screen: false }));
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("tui.{method} is not available to extensions in pi-rs")),
    })
}

/// JS keyspec (`string | string[]`) to [`pi_tui::keybindings::KeySpec`].
fn js_key_spec(value: &Value) -> Option<pi_tui::keybindings::KeySpec> {
    use pi_tui::keybindings::KeySpec;
    match value {
        Value::String(k) => Some(KeySpec::One(k.clone())),
        Value::Array(keys) => Some(KeySpec::Many(keys.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())),
        _ => None,
    }
}

/// `setKeybindings(manager)` from JS replaces pi-tui's global keybindings for the whole app, like
/// TS where extensions share pi-tui's module state: a proxy installs the Rust manager it stands
/// for, a JS `KeybindingsManager` is mirrored by a Rust manager with the same definitions and
/// user bindings (re-sent by the JS manager whenever it rebuilds).
fn keybindings_set_global(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    use pi_tui::keybindings::{KeybindingDefinition, KeybindingsManager as TuiKeybindingsManager};
    if let Some(id) = payload["proxy"].as_u64() {
        let manager = bridge.components.keybindings.get(id).ok_or_else(|| anyhow::anyhow!("unknown keybindings"))?;
        pi_tui::keybindings::set_keybindings(manager.tui_manager());
        return Ok(Value::Null);
    }
    let Some(definitions) = payload["definitions"].as_object() else {
        // The global proxy (`getKeybindings()` default) already is the global manager.
        return Ok(Value::Null);
    };
    let definitions = definitions
        .iter()
        .filter_map(|(id, definition)| {
            let default_keys = js_key_spec(&definition["defaultKeys"])?;
            let description = definition["description"].as_str().map(str::to_string);
            Some((id.clone(), KeybindingDefinition { default_keys, description }))
        })
        .collect();
    let user_bindings = payload["userBindings"]
        .as_object()
        .map(|bindings| bindings.iter().map(|(id, keys)| (id.clone(), js_key_spec(keys))).collect())
        .unwrap_or_default();
    pi_tui::keybindings::set_keybindings(Arc::new(TuiKeybindingsManager::new(definitions, user_bindings)));
    Ok(Value::Null)
}

fn keybindings_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    if payload["m"].as_str() == Some("setGlobal") {
        return keybindings_set_global(bridge, payload);
    }
    let manager = match payload["id"].as_u64() {
        Some(id) => bridge.components.keybindings.get(id).ok_or_else(|| anyhow::anyhow!("unknown keybindings"))?.tui_manager(),
        None => pi_tui::keybindings::get_keybindings(),
    };
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    Ok(match method.as_str() {
        "matches" => json!(manager.matches(&s(0), &s(1))),
        "getKeys" => json!(manager.get_keys(&s(0))),
        "getResolvedBindings" => {
            let mut out = serde_json::Map::new();
            for (id, spec) in manager.get_resolved_bindings() {
                out.insert(id, json!(spec.to_vec()));
            }
            Value::Object(out)
        }
        "getDefinition" => match manager.get_definition(&s(0)) {
            Some(definition) => {
                let mut out = json!({ "defaultKeys": match &definition.default_keys {
                    pi_tui::keybindings::KeySpec::One(k) => json!(k),
                    pi_tui::keybindings::KeySpec::Many(k) => json!(k),
                } });
                if let Some(description) = definition.description {
                    out["description"] = json!(description);
                }
                out
            }
            None => Value::Null,
        },
        _ => return Err(anyhow::anyhow!("keybindings.{method} is not available")),
    })
}

fn rust_component_new(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let kind = str_arg(payload, "kind");
    let options = &payload["options"];
    let comp = match kind.as_str() {
        "input" => RustComp::Input(Arc::new(Input::new(InputOptions {
            prompt: options["prompt"].as_str().map(str::to_string),
            placeholder: options["placeholder"].as_str().map(str::to_string),
            placeholder_style: js_style_fn(bridge, &options["placeholderStyle"]),
        }))),
        "editor" | "customEditor" => {
            let tui = payload["tui"]
                .as_u64()
                .and_then(|id| bridge.components.tuis.get(id))
                .ok_or_else(|| anyhow::anyhow!("Editor requires the tui passed to the component factory"))?;
            let theme = &payload["theme"];
            let editor_theme = EditorTheme {
                border_color: js_style_fn(bridge, &theme["borderColor"]).unwrap_or_else(identity_style),
                select_list: select_list_theme(bridge, &theme["selectList"]),
            };
            if kind == "customEditor" {
                let keybindings = payload["keybindings"]
                    .as_u64()
                    .and_then(|id| bridge.components.keybindings.get(id))
                    .unwrap_or_else(|| KeybindingsManager::create(None));
                let custom_options = CustomEditorOptions {
                    padding_x: options["paddingX"].as_i64(),
                    autocomplete_max_visible: options["autocompleteMaxVisible"].as_i64(),
                    embed_working_status: options["embedWorkingStatus"].as_bool(),
                };
                RustComp::CustomEditor(Arc::new(CustomEditor::new(tui, Arc::new(editor_theme), keybindings, Some(&custom_options))))
            } else {
                let editor_options = EditorOptions {
                    padding_x: options["paddingX"].as_u64().map(|v| v as usize),
                    autocomplete_max_visible: options["autocompleteMaxVisible"].as_u64().map(|v| v as usize),
                };
                RustComp::Editor(Editor::new(&tui, editor_theme, editor_options))
            }
        }
        other => match super::messages::new_component(bridge, other, payload)? {
            Some(component) => RustComp::Other(component),
            None => anyhow::bail!("unknown component kind {kind}"),
        },
    };
    Ok(json!(bridge.components.rust.insert(comp)))
}

fn text_callback(bridge: &Arc<Bridge>, value: &Value) -> Option<TextCallback> {
    let handle = bridge.handle(value)?;
    Some(Arc::new(move |text: String| {
        let _ = handle.call_blocking(vec![json!(text)]);
    }))
}

fn rust_component_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let comp = bridge.components.rust.get(id).ok_or_else(|| anyhow::anyhow!("unknown component"))?;
    let method = str_arg(payload, "m");
    let a = |i: usize| arg(payload, i);
    let component = comp.component();
    match method.as_str() {
        "render" => return Ok(lines_json(component.render(a(0).as_u64().unwrap_or(80) as usize))),
        "handleInput" => {
            component.handle_input(a(0).as_str().unwrap_or_default());
            return Ok(Value::Null);
        }
        "invalidate" => {
            component.invalidate();
            return Ok(Value::Null);
        }
        "handleMouse" => {
            let event = mouse_event_from(&a(0));
            return Ok(match component.handle_mouse(&event) {
                Some(r) => json!({ "handled": r.handled, "capture": r.capture, "focus": r.focus, "render": r.render }),
                None => Value::Null,
            });
        }
        "getFocused" => return Ok(json!(component.focused())),
        "setFocused" => {
            component.set_focused(a(0).as_bool().unwrap_or(false));
            return Ok(Value::Null);
        }
        _ => {}
    }
    match &comp {
        RustComp::Input(input) => Ok(match method.as_str() {
            "getValue" => json!(input.get_value()),
            "setValue" => {
                input.set_value(a(0).as_str().unwrap_or_default());
                Value::Null
            }
            "setOnSubmit" => {
                let cb = text_callback(bridge, &a(0));
                input.set_on_submit(cb);
                Value::Null
            }
            "setOnEscape" => {
                let cb = bridge.handle(&a(0)).map(|h| {
                    Arc::new(move || {
                        let _ = h.call_blocking(vec![]);
                    }) as pi_tui::components::input::EscapeFn
                });
                input.set_on_escape(cb);
                Value::Null
            }
            _ => return Err(anyhow::anyhow!("Input.{method} is not available")),
        }),
        RustComp::CustomEditor(custom) if method == "setActionHandler" => {
            let action: AppKeybinding = serde_json::from_value(a(0))?;
            let mut handlers = custom.action_handlers();
            match bridge.handle(&a(1)) {
                Some(h) => {
                    handlers.insert(action, Arc::new(move || {
                        let _ = h.call_blocking(vec![]);
                    }) as Arc<dyn Fn() + Send + Sync>);
                }
                None => {
                    handlers.shift_remove(&action);
                }
            }
            custom.set_action_handlers(handlers);
            Ok(Value::Null)
        }
        RustComp::CustomEditor(custom) if method.starts_with("set_on") => {
            let h = bridge.handle(&a(0));
            let void = h.clone().map(|h| {
                Arc::new(move || {
                    let _ = h.call_blocking(vec![]);
                }) as Arc<dyn Fn() + Send + Sync>
            });
            match method.as_str() {
                "set_onEscape" => custom.set_on_escape(void),
                "set_onCtrlD" => custom.set_on_ctrl_d(void),
                "set_onPasteImage" => custom.set_on_paste_image(void),
                "set_onExtensionShortcut" => custom.set_on_extension_shortcut(h.map(|h| {
                    Arc::new(move |data: String| {
                        h.call_blocking(vec![json!(data)]).ok().is_some_and(|v| v.as_bool().unwrap_or(false))
                    }) as Arc<dyn Fn(String) -> bool + Send + Sync>
                })),
                _ => return Err(anyhow::anyhow!("CustomEditor.{method} is not available")),
            }
            Ok(Value::Null)
        }
        RustComp::Editor(_) | RustComp::CustomEditor(_) => {
            let editor = comp.editor().cloned().unwrap_or_else(|| unreachable!("editor-backed component"));
            editor_call(bridge, &editor, &method, payload)
        }
        RustComp::Other(component) => match super::messages::call(component, &method, payload)? {
            Some(value) => Ok(value),
            None => Err(anyhow::anyhow!("component.{method} is not available")),
        },
    }
}

fn editor_call(bridge: &Arc<Bridge>, editor: &Arc<Editor>, method: &str, payload: &Value) -> anyhow::Result<Value> {
    let a = |i: usize| arg(payload, i);
    {
        Ok(match method {
            "setOnSubmit" => {
                editor.set_on_submit(text_callback(bridge, &a(0)));
                Value::Null
            }
            "setOnChange" => {
                editor.set_on_change(text_callback(bridge, &a(0)));
                Value::Null
            }
            "getDisableSubmit" => json!(editor.disable_submit()),
            "setDisableSubmit" => {
                editor.set_disable_submit(a(0).as_bool().unwrap_or(false));
                Value::Null
            }
            "setBorderColor" => {
                if let Some(f) = js_style_fn(bridge, &a(0)) {
                    editor.set_border_color(f);
                }
                Value::Null
            }
            "getText" => json!(editor.get_text()),
            "getExpandedText" => json!(editor.get_expanded_text()),
            "getLines" => json!(editor.get_lines()),
            "getCursor" => {
                let (line, col) = editor.get_cursor();
                json!({ "line": line, "col": col })
            }
            "setText" => {
                editor.set_text(a(0).as_str().unwrap_or_default());
                Value::Null
            }
            "insertTextAtCursor" => {
                editor.insert_text_at_cursor(a(0).as_str().unwrap_or_default());
                Value::Null
            }
            "addToHistory" => {
                editor.add_to_history(a(0).as_str().unwrap_or_default());
                Value::Null
            }
            "setPaddingX" => {
                editor.set_padding_x(a(0).as_u64().unwrap_or(0) as usize);
                Value::Null
            }
            "getPaddingX" => json!(editor.get_padding_x()),
            "setAutocompleteMaxVisible" => {
                editor.set_autocomplete_max_visible(a(0).as_u64().unwrap_or(5) as usize);
                Value::Null
            }
            "getAutocompleteMaxVisible" => json!(editor.get_autocomplete_max_visible()),
            "isShowingAutocomplete" => json!(editor.is_showing_autocomplete()),
            "setAutocompleteProvider" => {
                if let Some(id) = a(0)["$piAutocomplete"].as_u64()
                    && let Some(provider) = bridge.components.autocomplete.get(id)
                {
                    editor.set_autocomplete_provider(provider);
                }
                Value::Null
            }
            _ => return Err(anyhow::anyhow!("Editor.{method} is not available")),
        })
    }
}

fn tui_util(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let f = str_arg(payload, "fn");
    let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let s = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    let n = |i: usize| a(i).as_f64().unwrap_or(0.0).max(0.0) as usize;
    let _ = bridge;
    Ok(match f.as_str() {
        "visibleWidth" => json!(pi_tui::utils::visible_width(&s(0))),
        "truncateToWidth" => {
            let ellipsis = a(2).as_str().map(str::to_string).unwrap_or_else(|| "...".to_string());
            json!(pi_tui::utils::truncate_to_width(&s(0), n(1), &ellipsis, a(3).as_bool().unwrap_or(false)))
        }
        "wrapTextWithAnsi" => lines_json(pi_tui::utils::wrap_text_with_ansi(&s(0), n(1))),
        "sliceByColumn" => json!(pi_tui::utils::slice_by_column(&s(0), n(1), n(2), a(3).as_bool().unwrap_or(false))),
        "stripTerminalSequences" => json!(pi_tui::utils::strip_terminal_sequences(&s(0))),
        "matchesKey" => json!(pi_tui::keys::matches_key(&s(0), &s(1))),
        "parseKey" => json!(pi_tui::keys::parse_key(&s(0))),
        "isKeyRelease" => json!(pi_tui::keys::is_key_release(&s(0))),
        "isKeyRepeat" => json!(pi_tui::keys::is_key_repeat(&s(0))),
        "decodeKittyPrintable" => json!(pi_tui::keys::decode_kitty_printable(&s(0))),
        "isKittyProtocolActive" => json!(pi_tui::keys::is_kitty_protocol_active()),
        "fuzzyMatch" => {
            let m = pi_tui::fuzzy::fuzzy_match(&s(0), &s(1));
            json!({ "matches": m.matches, "score": m.score })
        }
        "imageFallback" => json!(pi_tui::terminal_image::image_fallback(&s(0), None, a(2).as_str())),
        "nativeClipboard" => match pi_tui::native_platform::get_native_clipboard() {
            Some(clipboard) => json!({ "setText": clipboard.can_set_text() }),
            None => Value::Null,
        },
        "utf16ToByte" => json!(utf16_to_byte(&s(0), n(1))),
        _ => match super::tui_helpers::dispatch(&f, &args) {
            Some(result) => result?,
            None => return Err(anyhow::anyhow!("pi-tui {f} is not available")),
        },
    })
}

/// `getNativeClipboard()` operations (`tui.clipboard`), over pi-tui's native clipboard.
pub(crate) async fn native_clipboard_call(payload: &Value) -> anyhow::Result<Value> {
    use base64::Engine;
    use pi_tui::native_platform::ClipboardRead;
    fn read<T>(result: Result<ClipboardRead<T>, String>, data: impl FnOnce(T) -> Value) -> anyhow::Result<Value> {
        Ok(match result.map_err(anyhow::Error::msg)? {
            ClipboardRead::Unavailable => json!({ "kind": "unavailable" }),
            ClipboardRead::Empty => json!({ "kind": "empty" }),
            ClipboardRead::Data(value) => json!({ "kind": "data", "data": data(value) }),
        })
    }
    let Some(clipboard) = pi_tui::native_platform::get_native_clipboard() else {
        return Ok(json!({ "kind": "unavailable" }));
    };
    match payload["m"].as_str().unwrap_or_default() {
        "getText" => read(clipboard.get_text().await, |text| json!(text)),
        "getImage" => read(clipboard.get_image().await, |bytes| json!(base64::engine::general_purpose::STANDARD.encode(bytes))),
        "getFilePaths" => match clipboard.get_file_paths() {
            Some(paths) => read(paths.await, |paths| json!(paths)),
            None => Ok(json!({ "kind": "unavailable" })),
        },
        "setText" => match clipboard.set_text(payload["text"].as_str().unwrap_or_default()) {
            Some(write) => {
                write.await.map_err(anyhow::Error::msg)?;
                Ok(Value::Null)
            }
            None => Err(anyhow::anyhow!("setText is not supported by the native clipboard on this platform")),
        },
        other => Err(anyhow::anyhow!("clipboard.{other} is not available")),
    }
}
