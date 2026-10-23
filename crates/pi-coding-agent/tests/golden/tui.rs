//! Builders for `tui.json` (scripts/golden/gen-tui.mjs): pi-tui base components with pi's themes.

use std::sync::Arc;

use pi_coding_agent::core::tools::renderers::with_built_in_renderers;
use pi_coding_agent::modes::interactive::components::assistant_message::AssistantMessageComponent;
use pi_coding_agent::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionComponentToolDefinition, ToolExecutionComponentUpdateResultResult,
};
use pi_coding_agent::modes::interactive::components::user_message::UserMessageComponent;
use pi_coding_agent::modes::interactive::theme::theme::{
    ThemeBg, ThemeColor, get_markdown_theme, get_select_list_theme, get_settings_list_theme, theme,
};
use pi_tui::StyleFn;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::image::{Image, ImageOptions, ImageTheme};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::components::loader::{CancellableLoader, Loader};
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::components::settings_list::{SettingItem, SettingsList, SettingsListOptions};
use pi_tui::components::spacer::Spacer;
use pi_tui::components::text::Text;
use pi_tui::components::truncated_text::TruncatedText;
use serde_json::{Value, json};

use super::harness::*;
use super::selectors::feed;

fn fg(color: ThemeColor) -> StyleFn {
    Arc::new(move |t: &str| theme().fg(&color, t))
}

fn bg(color: ThemeBg) -> StyleFn {
    Arc::new(move |t: &str| theme().bg(&color, t))
}

fn bg_named(name: &str) -> ThemeBg {
    serde_json::from_value(json!(name)).unwrap()
}

fn markdown(input: &Value) -> Markdown {
    let style = match opt_s(input, "style").unwrap_or("none") {
        "thinking" => Some(DefaultTextStyle { color: Some(fg(ThemeColor::ThinkingText)), italic: true, ..Default::default() }),
        "user" => Some(DefaultTextStyle {
            color: Some(fg(ThemeColor::UserMessageText)),
            bg_color: Some(bg(ThemeBg::UserMessageBg)),
            ..Default::default()
        }),
        _ => None,
    };
    let preserve = b(input, "preserve");
    Markdown::new(
        s(input, "text"),
        opt_i(input, "paddingX").unwrap_or(1) as usize,
        opt_i(input, "paddingY").unwrap_or(0) as usize,
        get_markdown_theme(),
        style,
        Some(MarkdownOptions {
            preserve_ordered_list_markers: preserve,
            preserve_backslash_escapes: preserve,
            ..Default::default()
        }),
    )
}

fn select_items(input: &Value) -> Vec<SelectItem> {
    input["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| SelectItem::new(s(i, "value"), s(i, "label"), opt_s(i, "description").map(str::to_string)))
        .collect()
}

fn setting_items(input: &Value) -> Vec<SettingItem> {
    input["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| SettingItem {
            id: s(i, "id").into(),
            label: s(i, "label").into(),
            description: opt_s(i, "description").map(str::to_string),
            current_value: s(i, "currentValue").into(),
            values: opt_from(i, "values"),
            submenu: None,
        })
        .collect()
}

pub fn build(kind: &str, input: &Value) -> Option<Built> {
    Some(match kind {
        "markdown" => component(markdown(input)),
        "select_list" => {
            let layout = input.get("layout").map(|l| SelectListLayoutOptions {
                min_primary_column_width: l.get("minPrimaryColumnWidth").and_then(Value::as_u64).map(|v| v as usize),
                max_primary_column_width: l.get("maxPrimaryColumnWidth").and_then(Value::as_u64).map(|v| v as usize),
                truncate_primary: None,
            });
            let list = SelectList::new(
                select_items(input),
                opt_i(input, "maxVisible").unwrap_or(5) as usize,
                (*get_select_list_theme()).clone(),
                layout.unwrap_or_default(),
            );
            if let Some(filter) = opt_s(input, "filter") {
                list.set_filter(filter);
            }
            if let Some(selected) = opt_i(input, "selected") {
                list.set_selected_index(selected as usize);
            }
            feed(&list, input);
            component(list)
        }
        "settings_list" => {
            let list = SettingsList::new(
                setting_items(input),
                opt_i(input, "maxVisible").unwrap_or(5) as usize,
                (*get_settings_list_theme()).clone(),
                Arc::new(|_, _| {}),
                Arc::new(|| {}),
                SettingsListOptions { enable_search: b(input, "search") },
            );
            feed(list.as_ref(), input);
            Built::Component(list)
        }
        "input" => {
            let options = match opt_s(input, "placeholder") {
                Some(p) => InputOptions {
                    placeholder: Some(p.into()),
                    placeholder_style: Some(fg(ThemeColor::Dim)),
                    ..Default::default()
                },
                None => InputOptions::default(),
            };
            let c = Input::new(options);
            if let Some(v) = opt_s(input, "value") {
                c.set_value(v);
            }
            if b(input, "focused") {
                pi_tui::tui::Component::set_focused(&c, true);
            }
            feed(&c, input);
            component(c)
        }
        "text" => {
            let bg_fn = opt_s(input, "bg").map(|name| bg(bg_named(name)));
            component(Text::new(s(input, "text"), opt_i(input, "px").unwrap() as usize, opt_i(input, "py").unwrap() as usize, bg_fn))
        }
        "box" => {
            let c = BoxComponent::new(2, 1, Some(bg(bg_named(s(input, "bg")))));
            let t = theme();
            c.add_child(Arc::new(Text::new(t.fg(&ThemeColor::CustomMessageLabel, "\x1b[1m[label]\x1b[22m"), 0, 0, None)));
            c.add_child(Arc::new(Spacer::new(1)));
            c.add_child(Arc::new(Text::new(
                "Inner text that is long enough to wrap inside the box at forty columns.",
                0,
                0,
                None,
            )));
            component(c)
        }
        "truncated_text" => component(TruncatedText::new(
            s(input, "text"),
            opt_i(input, "px").unwrap() as usize,
            opt_i(input, "py").unwrap() as usize,
        )),
        "loader" => Built::Component(Loader::new(&test_tui(), fg(ThemeColor::Accent), fg(ThemeColor::Muted), s(input, "message"), None)),
        "cancellable_loader" => Built::Component(CancellableLoader::new(
            &test_tui(),
            fg(ThemeColor::Accent),
            fg(ThemeColor::Muted),
            s(input, "message"),
            None,
        )),
        "image" => component(Image::new(
            s(input, "data"),
            s(input, "mime"),
            ImageTheme { fallback_color: fg(ThemeColor::Muted) },
            ImageOptions { filename: opt_s(input, "filename").map(str::to_string), ..Default::default() },
            None,
        )),
        "assistant_message" => component(AssistantMessageComponent::new(Some(from(input, "message")), None, None, None, None, None, None, None)),
        "user_message" => component(UserMessageComponent::new(s(input, "text"), None, None, None, None, None)),
        "tool_execution" => {
            let tool_name = s(input, "toolName");
            let definition = with_built_in_renderers(tool_name, None).map(ToolExecutionComponentToolDefinition::ToolRenderers);
            let c = ToolExecutionComponent::new(
                tool_name,
                "call-1",
                input["args"].clone(),
                None,
                definition.as_ref(),
                test_tui(),
                CWD,
            );
            let result: ToolExecutionComponentUpdateResultResult = serde_json::from_value(json!({
                "content": [{ "type": "text", "text": s(input, "result") }],
                "isError": false,
            }))
            .unwrap();
            c.update_result(&result, Some(false));
            c.set_expanded(true);
            component(c)
        }
        _ => return None,
    })
}
