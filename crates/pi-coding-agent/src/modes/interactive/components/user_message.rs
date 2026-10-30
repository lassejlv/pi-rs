//! Port of `packages/coding-agent/src/modes/interactive/components/user-message.ts`.

use crate::compat::MarkdownTheme;
use crate::core::extensions::types::MarkdownTransformer;
use crate::modes::interactive::components::assistant_message::{
    OSC133_ZONE_END, OSC133_ZONE_FINAL, OSC133_ZONE_START,
};
use crate::modes::interactive::components::markdown_transform::{
    CreateMarkdownTransformMessageType, markdown_transform_fn,
};
use crate::modes::interactive::theme::theme::{ThemeBg, ThemeColor, get_markdown_theme, theme};
use parking_lot::Mutex;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};
use pi_tui::tui::{Component, ComponentRef, Container, MouseEvent, MouseResult};
use std::sync::Arc;

/// Component that renders a user message
/// TS: `extends Container`
pub struct UserMessageComponent {
    container: Container,
    text: String,
    markdown_theme: MarkdownTheme,
    output_pad: Mutex<usize>,
    markdown_transformers: Vec<MarkdownTransformer>,
    message_id: Option<String>,
    timestamp: Option<String>,
}

impl UserMessageComponent {
    /// Defaults: `markdown_theme` [`get_markdown_theme`], `output_pad` 1, no transformers.
    pub fn new(
        text: &str,
        markdown_theme: Option<&MarkdownTheme>,
        output_pad: Option<i64>,
        markdown_transformers: Option<&[MarkdownTransformer]>,
        message_id: Option<&str>,
        timestamp: Option<&str>,
    ) -> Self {
        let component = Self {
            container: Container::new(),
            text: text.to_string(),
            markdown_theme: markdown_theme.cloned().unwrap_or_else(get_markdown_theme),
            output_pad: Mutex::new(output_pad.unwrap_or(1).max(0) as usize),
            markdown_transformers: markdown_transformers.map(<[_]>::to_vec).unwrap_or_default(),
            message_id: message_id.map(str::to_owned),
            timestamp: timestamp.map(str::to_owned),
        };
        component.rebuild();
        component
    }

    pub fn set_output_pad(&self, padding: i64) {
        *self.output_pad.lock() = padding.max(0) as usize;
        self.rebuild();
    }

    fn rebuild(&self) {
        self.container.clear();
        let output_pad = *self.output_pad.lock();
        // The Markdown pads and colors its own background: a Box around it would keep a second full-width copy of every
        // line, with identical output.
        self.container.add_child(Arc::new(Markdown::new(
            self.text.clone(),
            output_pad,
            1,
            self.markdown_theme.clone(),
            Some(DefaultTextStyle {
                color: Some(Arc::new(|content: &str| theme().fg(&ThemeColor::UserMessageText, content))),
                bg_color: Some(Arc::new(|content: &str| theme().bg(&ThemeBg::UserMessageBg, content))),
                ..Default::default()
            }),
            Some(MarkdownOptions {
                preserve_ordered_list_markers: true,
                preserve_backslash_escapes: true,
                transform: Some(markdown_transform_fn(
                    CreateMarkdownTransformMessageType::User,
                    false,
                    &self.markdown_transformers,
                    self.message_id.as_deref(),
                    self.timestamp.as_deref(),
                )),
                ..Default::default()
            }),
        )));
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.container.render(width);
        if lines.is_empty() {
            return lines;
        }
        lines[0] = format!("{OSC133_ZONE_START}{}", lines[0]);
        let last = lines.len() - 1;
        lines[last] = format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{}", lines[last]);
        lines
    }

    fn invalidate(&self) {
        self.container.invalidate();
    }

    fn handle_mouse(&self, event: &MouseEvent) -> Option<MouseResult> {
        self.container.handle_mouse(event)
    }

    fn child_components(&self) -> Option<Vec<ComponentRef>> {
        self.container.child_components()
    }
}
