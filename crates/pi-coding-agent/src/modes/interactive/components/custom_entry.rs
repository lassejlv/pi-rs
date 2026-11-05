//! Port of `packages/coding-agent/src/modes/interactive/components/custom-entry.ts`.

use crate::core::extensions::types::{EntryRenderOptions, EntryRenderer};
use crate::core::session_manager::CustomEntry;
use crate::modes::interactive::theme::theme::{ThemeBg, ThemeColor, theme};
use parking_lot::Mutex;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::tui::{Component, ComponentRef, Container, MouseEvent, MouseResult};
use std::sync::Arc;

struct State {
    custom_component: Option<ComponentRef>,
    expanded: bool,
    output_pad: usize,
}

/// Component that renders a custom session entry from extensions.
/// The host owns transcript spacing; renderer output should provide only its content.
/// TS: `extends Container`
pub struct CustomEntryComponent {
    container: Container,
    entry: CustomEntry,
    renderer: EntryRenderer,
    st: Mutex<State>,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown error".to_string()
    }
}

impl CustomEntryComponent {
    pub fn new(entry: &CustomEntry, renderer: &EntryRenderer, output_pad: Option<i64>) -> Self {
        let component = Self {
            container: Container::new(),
            entry: entry.clone(),
            renderer: renderer.clone(),
            st: Mutex::new(State {
                custom_component: None,
                expanded: false,
                output_pad: output_pad.unwrap_or(1).max(0) as usize,
            }),
        };
        component.rebuild();
        component
    }

    pub fn has_content(&self) -> bool {
        self.st.lock().custom_component.is_some()
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
        self.st.lock().output_pad = output_pad.max(0) as usize;
        self.rebuild();
    }

    fn rebuild(&self) {
        self.container.clear();
        let (expanded, output_pad) = {
            let mut st = self.st.lock();
            st.custom_component = None;
            (st.expanded, st.output_pad)
        };

        let t = theme();
        let options = EntryRenderOptions { expanded };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.renderer)(&self.entry, &options, &t)));
        let failure = |payload: Result<Option<ComponentRef>, Box<dyn std::any::Any + Send>>| match payload {
            Ok(Some(component)) => {
                crate::core::extensions::types::render_failure(&component).map(str::to_string).ok_or(Some(component))
            }
            Ok(None) => Err(None),
            Err(payload) => Ok(panic_message(payload.as_ref())),
        };
        let component: Option<ComponentRef> = match failure(result) {
            Err(component) => component,
            Ok(message) => {
                let box_component = BoxComponent::new(
                    output_pad,
                    1,
                    Some(Arc::new(|text: &str| theme().bg(&ThemeBg::CustomMessageBg, text))),
                );
                box_component.add_child(Arc::new(Text::new(
                    t.fg(&ThemeColor::Error, &format!("[{}] renderer failed: {message}", self.entry.custom_type)),
                    0,
                    0,
                    None,
                )));
                Some(Arc::new(box_component))
            }
        };

        let Some(component) = component else {
            return;
        };

        self.st.lock().custom_component = Some(component.clone());
        self.container.add_child(Arc::new(Spacer::new(1)));
        self.container.add_child(component);
    }
}

impl Component for CustomEntryComponent {
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
