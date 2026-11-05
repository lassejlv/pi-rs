//! Port of `packages/coding-agent/src/modes/interactive/components/custom-message.ts`.

use crate::compat::MarkdownTheme;
use crate::core::extensions::types::{MessageRenderOptions, MessageRenderer};
use crate::core::messages::CustomMessage;
use crate::modes::interactive::theme::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use parking_lot::Mutex;
use pi_ai::types::{UserContent, UserContentBlock};
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown};
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::tui::{Component, ComponentRef, Container, MouseEvent, MouseResult};
use std::sync::Arc;

struct State {
    custom_component: Option<ComponentRef>,
    expanded: bool,
    output_pad: usize,
}

/// Component that renders a custom message entry from extensions.
/// Uses distinct styling to differentiate from user messages.
/// TS: `extends Container`
pub struct CustomMessageComponent {
    container: Container,
    message: CustomMessage,
    custom_renderer: Option<MessageRenderer>,
    box_component: Arc<BoxComponent>,
    markdown_theme: MarkdownTheme,
    st: Mutex<State>,
}

impl CustomMessageComponent {
    pub fn new(
        message: &CustomMessage,
        custom_renderer: Option<&MessageRenderer>,
        markdown_theme: Option<&MarkdownTheme>,
        output_pad: Option<i64>,
    ) -> Self {
        let container = Container::new();
        container.add_child(Arc::new(Spacer::new(1)));
        // Create box with purple background (used for default rendering)
        let box_component =
            Arc::new(BoxComponent::new(1, 1, Some(Arc::new(|t: &str| theme().bg(&ThemeBg::CustomMessageBg, t)))));
        let component = Self {
            container,
            message: message.clone(),
            custom_renderer: custom_renderer.cloned(),
            box_component,
            markdown_theme: markdown_theme.cloned().unwrap_or_else(get_markdown_theme),
            st: Mutex::new(State {
                custom_component: None,
                expanded: false,
                output_pad: output_pad.unwrap_or(1).max(0) as usize,
            }),
        };
        component.rebuild();
        component
    }

    pub fn set_expanded(&self, expanded: bool) {
        let changed = {
            let mut st = self.st.lock();
            let changed = st.expanded != expanded;
            st.expanded = expanded;
            changed
        };
        if changed {
            self.rebuild();
        }
    }

    pub fn set_output_pad(&self, output_pad: i64) {
        let output_pad = output_pad.max(0) as usize;
        let changed = {
            let mut st = self.st.lock();
            let changed = st.output_pad != output_pad;
            st.output_pad = output_pad;
            changed
        };
        if changed {
            self.rebuild();
        }
    }

    fn rebuild(&self) {
        // Remove previous content component
        let (previous, expanded, output_pad) = {
            let mut st = self.st.lock();
            (st.custom_component.take(), st.expanded, st.output_pad)
        };
        if let Some(previous) = previous {
            self.container.remove_child(&previous);
        }
        let box_ref: ComponentRef = self.box_component.clone();
        self.container.remove_child(&box_ref);

        // Try custom renderer first - it handles its own styling
        if let Some(renderer) = &self.custom_renderer {
            let options = MessageRenderOptions { expanded, output_pad };
            let t = theme();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| renderer(&self.message, &options, &t)));
            // Fall through to default rendering when the renderer fails or returns nothing.
            if let Ok(Some(component)) = result
                && crate::core::extensions::types::render_failure(&component).is_none()
            {
                // Custom renderer provides its own styled component
                self.st.lock().custom_component = Some(component.clone());
                self.container.add_child(component);
                return;
            }
        }

        // Default rendering uses our box
        self.container.add_child(box_ref);
        self.box_component.clear();
        self.box_component.set_padding_x(output_pad);

        // Default rendering: label + content
        let t = theme();
        let label = t.fg(&ThemeColor::CustomMessageLabel, &format!("\x1b[1m[{}]\x1b[22m", self.message.custom_type));
        self.box_component.add_child(Arc::new(Text::new(label, 0, 0, None)));
        self.box_component.add_child(Arc::new(Spacer::new(1)));

        // Extract text content
        let text = match &self.message.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|c| match c {
                    UserContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };

        self.box_component.add_child(Arc::new(Markdown::new(
            text,
            0,
            0,
            self.markdown_theme.clone(),
            Some(DefaultTextStyle {
                color: Some(Arc::new(|text: &str| theme().fg(&ThemeColor::CustomMessageText, text))),
                ..Default::default()
            }),
            None,
        )));
    }
}

impl Component for CustomMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&self) {
        self.container.invalidate();
        self.rebuild();
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.container.handle_mouse(event)
    }

    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        self.container.child_components()
    }
}
