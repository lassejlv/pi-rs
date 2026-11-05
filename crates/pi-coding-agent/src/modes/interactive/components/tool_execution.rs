//! Port of `packages/coding-agent/src/modes/interactive/components/tool-execution.ts`.

use crate::core::extensions::types::ToolDefinition;
use crate::core::extensions::types::ToolRenderers;
use crate::core::extensions::types::{RenderShell, ToolRenderContext, ToolRenderResultOptions, ToolRenderState};
use crate::core::tools::render_utils::{
    GetTextOutputResult, GetTextOutputResultContent, format_tool_call_with_args_ref, get_text_output,
};
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::theme::theme::{ThemeBg, ThemeColor, theme};
use crate::utils::image_convert::ensure_png_transcoder;
use parking_lot::Mutex;
use pi_agent::types::AgentToolResult;
use pi_ai::types::{ImageContent, UserContentBlock};
use pi_tui::StyleFn;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::image::{Image, ImageOptions, ImageTheme};
use pi_tui::components::mouse_region::MouseRegion;
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::terminal_image::get_capabilities;
use pi_tui::tui::Component;
use pi_tui::tui::Tui;
use pi_tui::tui::{ComponentRef, Container, MouseButton, MouseEvent, MouseEventType, MouseResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Weak};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionComponentUpdateResultResultContent {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionComponentUpdateResultResult {
    pub content: Vec<ToolExecutionComponentUpdateResultResultContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    /// Execution time of a final result.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "pi_ai::serde_util::opt_js_f64"
    )]
    pub duration_ms: Option<f64>,
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ToolExecutionComponentToolDefinition {
    ToolRenderers(ToolRenderers),
    ToolDefinition(ToolDefinition),
}

const FALLBACK_PREVIEW_LINES: usize = 10;

#[derive(Clone, PartialEq)]
struct ImageSource {
    data: String,
    mime_type: String,
    width_cells: usize,
}

struct State {
    self_render_height: usize,
    call_renderer_component: Option<ComponentRef>,
    result_renderer_component: Option<ComponentRef>,
    image_components: Vec<ComponentRef>,
    /// Inputs of image_components, so update_display can reuse images and keep their converted PNG data.
    image_sources: Vec<ImageSource>,
    image_spacers: Vec<ComponentRef>,
    args: Value,
    expanded: bool,
    show_images: bool,
    image_width_cells: usize,
    output_pad: usize,
    is_partial: bool,
    execution_started: bool,
    args_complete: bool,
    result: Option<ToolExecutionComponentUpdateResultResult>,
    hide_component: bool,
}

struct Inner {
    weak_self: Weak<Inner>,
    container: Container,
    content_box: Arc<BoxComponent>,
    content_text: Arc<Text>,
    content_text_region: ComponentRef,
    self_render_container: Arc<Container>,
    renderer_state: ToolRenderState,
    tool_name: String,
    tool_call_id: String,
    tool_definition: Option<ToolRenderers>,
    ui: Weak<Tui>,
    cwd: String,
    st: Mutex<State>,
}

/// TS: `extends Container`
pub struct ToolExecutionComponent {
    inner: Arc<Inner>,
}

fn bg_fn(bg: ThemeBg) -> StyleFn {
    Arc::new(move |text: &str| theme().bg(&bg, text))
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        tool_call_id: &str,
        args: Value,
        options: Option<&ToolExecutionOptions>,
        tool_definition: Option<&ToolExecutionComponentToolDefinition>,
        ui: Arc<Tui>,
        cwd: &str,
    ) -> Self {
        let options = options.cloned().unwrap_or_default();
        let tool_definition = tool_definition.map(|d| match d {
            ToolExecutionComponentToolDefinition::ToolRenderers(r) => r.clone(),
            ToolExecutionComponentToolDefinition::ToolDefinition(d) => d.renderers(),
        });
        let content_text = Arc::new(Text::new("", 1, 1, Some(bg_fn(ThemeBg::ToolPendingBg))));
        let inner = Arc::new_cyclic(|weak: &Weak<Inner>| {
            let content_text_region = create_result_region(weak.clone(), content_text.clone());
            Inner {
                weak_self: weak.clone(),
                container: Container::new(),
                // Always create all shell variants. contentBox is used for default renderer-based composition.
                // selfRenderContainer is used when the tool renders its own framing.
                // contentText is reserved for generic fallback rendering when no tool definition exists.
                content_box: Arc::new(BoxComponent::new(1, 1, Some(bg_fn(ThemeBg::ToolPendingBg)))),
                content_text,
                content_text_region,
                self_render_container: Arc::new(Container::new()),
                renderer_state: Arc::new(parking_lot::Mutex::new(None)),
                tool_name: tool_name.to_string(),
                tool_call_id: tool_call_id.to_string(),
                tool_definition,
                ui: Arc::downgrade(&ui),
                cwd: cwd.to_string(),
                st: Mutex::new(State {
                    self_render_height: 0,
                    call_renderer_component: None,
                    result_renderer_component: None,
                    image_components: Vec::new(),
                    image_sources: Vec::new(),
                    image_spacers: Vec::new(),
                    args,
                    expanded: false,
                    show_images: options.show_images.unwrap_or(true),
                    image_width_cells: options.image_width_cells.unwrap_or(60).max(0) as usize,
                    output_pad: options.output_pad.unwrap_or(1).max(0) as usize,
                    is_partial: true,
                    execution_started: false,
                    args_complete: false,
                    result: None,
                    hide_component: false,
                }),
            }
        });

        inner.container.add_child(Arc::new(Spacer::new(1)));
        if inner.has_renderer_definition() {
            if inner.render_shell() == RenderShell::SelfRendered {
                inner.container.add_child(inner.self_render_container.clone() as ComponentRef);
            } else {
                inner.container.add_child(inner.content_box.clone() as ComponentRef);
            }
        } else {
            inner.container.add_child(inner.content_text_region.clone());
        }

        inner.update_display();
        Self { inner }
    }

    pub fn update_args(&self, args: Value) {
        self.inner.st.lock().args = args;
        self.inner.update_display();
    }

    pub fn mark_execution_started(&self) {
        self.inner.st.lock().execution_started = true;
        self.inner.update_display();
        self.inner.request_render();
    }

    pub fn set_args_complete(&self) {
        self.inner.st.lock().args_complete = true;
        self.inner.update_display();
        self.inner.request_render();
    }

    /// `is_partial` defaults to false.
    pub fn update_result(
        &self,
        result: &ToolExecutionComponentUpdateResultResult,
        is_partial: Option<bool>,
    ) {
        {
            let mut st = self.inner.st.lock();
            st.result = Some(result.clone());
            st.is_partial = is_partial.unwrap_or(false);
        }
        self.inner.update_display();
    }

    pub fn set_expanded(&self, expanded: bool) {
        self.inner.set_expanded(expanded);
    }

    pub fn set_output_pad(&self, output_pad: i64) {
        self.inner.st.lock().output_pad = output_pad.max(0) as usize;
        self.inner.update_display();
    }

    pub fn set_show_images(&self, show: bool) {
        self.inner.st.lock().show_images = show;
        self.inner.update_display();
    }

    pub fn set_image_width_cells(&self, width: i64) {
        self.inner.st.lock().image_width_cells = width.max(1) as usize;
        self.inner.update_display();
    }
}

fn create_result_region(weak: Weak<Inner>, component: ComponentRef) -> ComponentRef {
    Arc::new(MouseRegion::new(
        component,
        Arc::new(move |event: &MouseEvent| {
            let inner = weak.upgrade()?;
            let (has_result, expanded) = {
                let st = inner.st.lock();
                (st.result.is_some(), st.expanded)
            };
            if !has_result || event.kind != MouseEventType::Click || event.button != MouseButton::Left {
                return None;
            }
            inner.set_expanded(!expanded);
            Some(MouseResult::handled())
        }),
    ))
}

fn result_content(result: &ToolExecutionComponentUpdateResultResult) -> Vec<GetTextOutputResultContent> {
    result
        .content
        .iter()
        .map(|c| GetTextOutputResultContent {
            r#type: c.r#type.clone(),
            text: c.text.clone(),
            data: c.data.clone(),
            mime_type: c.mime_type.clone(),
        })
        .collect()
}

impl Inner {
    fn request_render(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.request_render(false);
        }
    }

    fn set_expanded(&self, expanded: bool) {
        self.st.lock().expanded = expanded;
        self.update_display();
    }

    fn invalidate(&self) {
        self.container.invalidate();
        self.update_display();
    }

    fn has_renderer_definition(&self) -> bool {
        self.tool_definition.is_some()
    }

    fn render_shell(&self) -> RenderShell {
        self.tool_definition.as_ref().and_then(|d| d.render_shell).unwrap_or_default()
    }

    fn render_context(&self, last_component: Option<ComponentRef>) -> ToolRenderContext {
        let st = self.st.lock();
        let weak = self.weak_self.clone();
        ToolRenderContext {
            args: st.args.clone(),
            tool_call_id: self.tool_call_id.clone(),
            invalidate: Arc::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.invalidate();
                    inner.request_render();
                }
            }),
            last_component,
            state: self.renderer_state.clone(),
            cwd: self.cwd.clone(),
            execution_started: st.execution_started,
            args_complete: st.args_complete,
            is_partial: st.is_partial,
            expanded: st.expanded,
            show_images: st.show_images,
            is_error: st.result.as_ref().is_some_and(|r| r.is_error),
            duration_ms: if st.is_partial {
                None
            } else {
                st.result.as_ref().and_then(|r| r.duration_ms).map(|d| d.max(0.0) as u64)
            },
            output_pad: st.output_pad,
        }
    }

    fn create_call_fallback(&self) -> ComponentRef {
        let (args, expanded) = {
            let st = self.st.lock();
            (st.args.clone(), st.expanded)
        };
        let t = theme();
        Arc::new(Text::new(format_tool_call_with_args_ref(&self.tool_name, Some(&args), &t, expanded), 0, 0, None))
    }

    fn create_result_fallback(&self) -> Option<ComponentRef> {
        let output = self.text_output();
        if output.is_empty() {
            return None;
        }
        let expanded = self.st.lock().expanded;
        let t = theme();

        let lines: Vec<&str> = output.split('\n').collect();
        let display_lines = if expanded { &lines[..] } else { &lines[..lines.len().min(FALLBACK_PREVIEW_LINES)] };
        let remaining = lines.len() - display_lines.len();
        let mut text =
            display_lines.iter().map(|line| t.fg(&ThemeColor::ToolOutput, line)).collect::<Vec<_>>().join("\n");
        if remaining > 0 {
            text.push_str(&format!(
                "{} {}{}",
                t.fg(&ThemeColor::Muted, &format!("\n... ({remaining} more lines,")),
                key_hint("app.tools.expand".to_string(), "to expand"),
                t.fg(&ThemeColor::Muted, ")")
            ));
        }
        Some(Arc::new(Text::new(text, 0, 0, None)))
    }

    // `has_content` mirrors the TS flow, where the call slot always produces content.
    #[allow(unused_assignments)]
    fn update_display(&self) {
        let (is_partial, is_error, output_pad, result, show_images, image_width_cells) = {
            let mut st = self.st.lock();
            st.hide_component = false;
            (
                st.is_partial,
                st.result.as_ref().is_some_and(|r| r.is_error),
                st.output_pad,
                st.result.clone(),
                st.show_images,
                st.image_width_cells,
            )
        };
        let bg = if is_partial {
            bg_fn(ThemeBg::ToolPendingBg)
        } else if is_error {
            bg_fn(ThemeBg::ToolErrorBg)
        } else {
            bg_fn(ThemeBg::ToolSuccessBg)
        };

        let mut has_content = false;
        if let Some(definition) = &self.tool_definition {
            let self_rendered = self.render_shell() == RenderShell::SelfRendered;
            let add: Box<dyn Fn(ComponentRef)> = if self_rendered {
                self.self_render_container.clear();
                let c = self.self_render_container.clone();
                Box::new(move |child| c.add_child(child))
            } else {
                self.content_box.set_bg_fn(Some(bg.clone()));
                self.content_box.set_padding_x(output_pad);
                self.content_box.clear();
                let c = self.content_box.clone();
                Box::new(move |child| c.add_child(child))
            };

            match &definition.render_call {
                None => {
                    add(create_result_region(self.weak_self.clone(), self.create_call_fallback()));
                    has_content = true;
                }
                Some(call_renderer) => {
                    let last = self.st.lock().call_renderer_component.clone();
                    let context = self.render_context(last);
                    let args = context.args.clone();
                    let t = theme();
                    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        call_renderer(&args, &t, &context)
                    }));
                    match rendered {
                        Ok(component) if crate::core::extensions::types::render_failure(&component).is_none() => {
                            self.st.lock().call_renderer_component = Some(component.clone());
                            add(create_result_region(self.weak_self.clone(), component));
                        }
                        _ => {
                            self.st.lock().call_renderer_component = None;
                            add(create_result_region(self.weak_self.clone(), self.create_call_fallback()));
                        }
                    }
                    has_content = true;
                }
            }

            if let Some(result) = &result {
                match &definition.render_result {
                    None => {
                        if let Some(component) = self.create_result_fallback() {
                            add(create_result_region(self.weak_self.clone(), component));
                            has_content = true;
                        }
                    }
                    Some(result_renderer) => {
                        let last = self.st.lock().result_renderer_component.clone();
                        let context = self.render_context(last);
                        let tool_result = AgentToolResult {
                            content: result
                                .content
                                .iter()
                                .filter_map(|c| match c.r#type.as_str() {
                                    "text" => Some(UserContentBlock::text(c.text.clone().unwrap_or_default())),
                                    "image" => Some(UserContentBlock::Image(ImageContent {
                                        data: c.data.clone().unwrap_or_default(),
                                        mime_type: c.mime_type.clone().unwrap_or_default(),
                                    })),
                                    _ => None,
                                })
                                .collect(),
                            details: result.details.clone(),
                            ..Default::default()
                        };
                        let options = ToolRenderResultOptions { expanded: context.expanded, is_partial };
                        let t = theme();
                        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            result_renderer(&tool_result, options, &t, &context)
                        }));
                        match rendered {
                            Ok(component) if crate::core::extensions::types::render_failure(&component).is_none() => {
                                self.st.lock().result_renderer_component = Some(component.clone());
                                add(create_result_region(self.weak_self.clone(), component));
                                has_content = true;
                            }
                            _ => {
                                self.st.lock().result_renderer_component = None;
                                if let Some(component) = self.create_result_fallback() {
                                    add(create_result_region(self.weak_self.clone(), component));
                                    has_content = true;
                                }
                            }
                        }
                    }
                }
            }
        } else {
            self.content_text.set_custom_bg_fn(Some(bg));
            self.content_text.set_padding_x(output_pad);
            self.content_text.set_text(self.format_tool_execution());
            has_content = true;
        }

        let (previous_images, previous_sources, previous_spacers) = {
            let mut st = self.st.lock();
            (
                std::mem::take(&mut st.image_components),
                std::mem::take(&mut st.image_sources),
                std::mem::take(&mut st.image_spacers),
            )
        };
        for img in &previous_images {
            self.container.remove_child(img);
        }
        for spacer in &previous_spacers {
            self.container.remove_child(spacer);
        }

        let mut image_components: Vec<ComponentRef> = Vec::new();
        let mut image_sources: Vec<ImageSource> = Vec::new();
        let mut image_spacers: Vec<ComponentRef> = Vec::new();
        let mut needs_transcoder = false;
        if let Some(result) = &result {
            let caps = get_capabilities();
            for img in result.content.iter().filter(|c| c.r#type == "image") {
                let (Some(data), Some(mime_type)) = (img.data.as_ref(), img.mime_type.as_ref()) else {
                    continue;
                };
                if caps.images.is_none() || !show_images || data.is_empty() || mime_type.is_empty() {
                    continue;
                }
                let spacer: ComponentRef = Arc::new(Spacer::new(1));
                self.container.add_child(spacer.clone());
                image_spacers.push(spacer);
                let source = ImageSource { data: data.clone(), mime_type: mime_type.clone(), width_cells: image_width_cells };
                let index = image_components.len();
                let image_component: ComponentRef = match (previous_sources.get(index), previous_images.get(index)) {
                    (Some(previous), Some(previous_image)) if *previous == source => previous_image.clone(),
                    _ => Arc::new(Image::new(
                        source.data.clone(),
                        source.mime_type.clone(),
                        ImageTheme { fallback_color: Arc::new(|s: &str| theme().fg(&ThemeColor::ToolOutput, s)) },
                        ImageOptions { max_width_cells: Some(source.width_cells), ..Default::default() },
                        None,
                    )),
                };
                if source.mime_type != "image/png" {
                    needs_transcoder = true;
                }
                self.container.add_child(image_component.clone());
                image_components.push(image_component);
                image_sources.push(source);
            }
        }
        let has_images = !image_components.is_empty();
        {
            let mut st = self.st.lock();
            st.image_components = image_components;
            st.image_sources = image_sources;
            st.image_spacers = image_spacers;
            if self.has_renderer_definition() && !has_content && !has_images {
                st.hide_component = true;
            }
        }

        if needs_transcoder {
            let weak = self.weak_self.clone();
            let ui = self.ui.clone();
            ensure_png_transcoder(Arc::new(move || {
                // Re-render on the UI loop: registration may complete while this update runs.
                let weak = weak.clone();
                let task = move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.invalidate();
                        inner.request_render();
                    }
                };
                match ui.upgrade() {
                    Some(ui) => ui.event_loop().post(task),
                    None => pi_tui::event_loop::current().post(task),
                }
            }));
        }
    }

    fn text_output(&self) -> String {
        let (result, show_images) = {
            let st = self.st.lock();
            (st.result.clone(), st.show_images)
        };
        let result = result.map(|r| GetTextOutputResult { content: result_content(&r) });
        get_text_output(result.as_ref(), show_images)
    }

    fn format_tool_execution(&self) -> String {
        let t = theme();
        let mut text = t.fg(&ThemeColor::ToolTitle, &t.bold(&self.tool_name));
        let args = self.st.lock().args.clone();
        if let Ok(content) = serde_json::to_string_pretty(&args) {
            text.push_str(&format!("\n\n{content}"));
        }
        let output = self.text_output();
        if !output.is_empty() {
            text.push_str(&format!("\n{output}"));
        }
        text
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let inner = &self.inner;
        let (hide, images, spacers) = {
            let st = inner.st.lock();
            (st.hide_component, st.image_components.clone(), st.image_spacers.clone())
        };
        if hide {
            return Vec::new();
        }

        if inner.has_renderer_definition() && inner.render_shell() == RenderShell::SelfRendered {
            let content_lines = inner.self_render_container.render(width);
            inner.st.lock().self_render_height = content_lines.len();
            if content_lines.is_empty() && images.is_empty() {
                return Vec::new();
            }

            let mut lines: Vec<String> = Vec::new();
            if !content_lines.is_empty() {
                lines.push(String::new());
                lines.extend(content_lines);
            }
            for (i, image) in images.iter().enumerate() {
                if let Some(spacer) = spacers.get(i) {
                    lines.extend(spacer.render(width));
                }
                lines.extend(image.render(width));
            }
            return lines;
        }

        inner.container.render(width)
    }

    fn invalidate(&self) {
        self.inner.invalidate();
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        let inner = &self.inner;
        if !inner.has_renderer_definition() || inner.render_shell() != RenderShell::SelfRendered {
            return inner.container.handle_mouse(event);
        }
        let height = inner.st.lock().self_render_height as i64;
        if event.y <= 0 || event.y > height {
            return None;
        }
        inner.self_render_container.handle_mouse(&MouseEvent { y: event.y - 1, height, ..*event })
    }

    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        self.inner.container.child_components()
    }
}
