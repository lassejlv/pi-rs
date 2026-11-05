//! Tests for the message, tool and dialog components, ported from
//! `packages/coding-agent/test/{assistant-message,user-message,custom-message,
//! collapsible-message-components,output-pad,themed-text,status-indicator,bash-execution-width,
//! footer-width,mermaid,tool-execution-component,custom-editor-history-keybindings}.test.ts`.

use crate::core::agent_session::{AgentSessionRoutedModel, ParsedSkillBlock};
use crate::core::extensions::types::{
    ContextUsage, MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer, MessageRenderOptions,
    MessageRenderer, RenderShell, ToolDefinition, ToolRenderers,
};
use crate::core::footer_data_provider::FooterDataProvider;
use crate::core::keybindings::KeybindingsManager;
use crate::core::messages::{BranchSummaryMessage, CompactionSummaryMessage, CustomMessage};
use crate::core::session_manager::SessionEntry;
use crate::core::settings_schema::MermaidRenderingMode;
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use crate::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use crate::modes::interactive::components::custom_editor::{CustomEditor, CustomEditorOptions};
use crate::modes::interactive::components::custom_message::CustomMessageComponent;
use crate::modes::interactive::components::footer::{FooterComponent, FooterSession, format_cwd_for_footer};
use crate::modes::interactive::components::mermaid::{MermaidTransformerOptions, create_mermaid_markdown_transformer};
use crate::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use crate::modes::interactive::components::status_indicator::{
    BranchSummaryStatusIndicator, CompactionStatusIndicator, CompactionStatusReason, IdleStatus, RetryStatusIndicator,
    StatusIndicator, WorkingStatusIndicator,
};
use crate::modes::interactive::components::themed_text::ThemedText;
use crate::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionComponentToolDefinition, ToolExecutionComponentUpdateResultResult,
    ToolExecutionComponentUpdateResultResultContent, ToolExecutionOptions,
};
use crate::modes::interactive::components::user_message::UserMessageComponent;
use crate::modes::interactive::theme::theme::{
    Theme, ThemeColor, get_editor_theme, get_theme_by_name, init_theme, theme,
};
use crate::utils::ansi::strip_ansi;
use parking_lot::{Mutex, MutexGuard};
use pi_agent::types::{AgentToolResult, ThinkingLevel};
use pi_ai::types::{AssistantMessage, Model};
use pi_tui::components::editor::EditorComponent;
use pi_tui::components::text::Text;
use pi_tui::event_loop::EventLoop;
use pi_tui::testing::VirtualTerminal;
use pi_tui::tui::{Component, ComponentRef, MouseButton, MouseEvent, MouseEventType, Tui, TuiOptions};
use pi_tui::utils::visible_width;
use serde_json::{Value, json};
use std::sync::Arc;

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";
const OSC133_ZONE_FINAL: &str = "\x1b]133;C\x07";

static THEME_LOCK: Mutex<()> = Mutex::new(());

/// Serializes tests that depend on the global theme, and selects the dark theme.
fn dark_theme() -> MutexGuard<'static, ()> {
    let guard = THEME_LOCK.lock();
    init_theme(Some("dark"), Some(false));
    guard
}

fn strip(s: &str) -> String {
    strip_ansi(s).unwrap()
}

fn strip_lines(lines: &[String]) -> String {
    strip(&lines.join("\n"))
}

/// A TUI on a virtual terminal, driven by a manual event loop (fake timers).
fn test_tui(columns: usize, rows: usize) -> (Arc<Tui>, EventLoop) {
    let event_loop = EventLoop::new_manual();
    let tui = Tui::main_screen_with(
        Arc::new(VirtualTerminal::new(columns, rows)),
        TuiOptions { event_loop: Some(event_loop.clone()), ..Default::default() },
    );
    (tui, event_loop)
}

fn click(x: i64, y: i64, width: i64, height: i64) -> MouseEvent {
    MouseEvent {
        kind: MouseEventType::Click,
        button: MouseButton::Left,
        x,
        y,
        screen_x: x,
        screen_y: y,
        width,
        height,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: Some(1),
    }
}

fn assistant_message(content: Value, stop_reason: &str) -> AssistantMessage {
    serde_json::from_value(json!({
        "role": "assistant",
        "content": content,
        "api": "openai-responses",
        "provider": "openai",
        "model": "gpt-4o-mini",
        "usage": {
            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
        },
        "stopReason": stop_reason,
        "timestamp": 0
    }))
    .unwrap()
}

fn transformer(f: impl Fn(&str, &MarkdownTransformContext) -> String + Send + Sync + 'static) -> MarkdownTransformer {
    Arc::new(f)
}

// ---------------------------------------------------------------------------
// assistant-message.test.ts
// ---------------------------------------------------------------------------

#[test]
fn assistant_adds_osc133_zone_markers_without_tool_calls() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(json!([{"type": "text", "text": "hello"}]), "stop")),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let lines = component.render(40);
    assert!(!lines.is_empty());
    assert!(lines[0].contains(OSC133_ZONE_START));
    assert!(lines.last().unwrap().starts_with(&format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}")));
}

#[test]
fn assistant_has_no_zone_markers_with_tool_calls() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(
            json!([
                {"type": "text", "text": "calling tool"},
                {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "file.txt"}}
            ]),
            "stop",
        )),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let rendered = component.render(60).join("\n");
    assert!(!rendered.contains(OSC133_ZONE_START));
    assert!(!rendered.contains(OSC133_ZONE_END));
    assert!(!rendered.contains(OSC133_ZONE_FINAL));
}

#[test]
fn assistant_renders_length_stops_with_neutral_wording() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(json!([{"type": "thinking", "thinking": "private reasoning"}]), "length")),
        Some(true),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let rendered = component.render(80).join("\n");
    assert!(rendered.contains("Thinking..."));
    assert!(rendered.contains("Response was truncated before completion."));
}

#[test]
fn assistant_coalesces_adjacent_thinking_blocks() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(
            json!([
                {"type": "thinking", "thinking": "first thought"},
                {"type": "thinking", "thinking": ""},
                {"type": "thinking", "thinking": "second thought"},
                {"type": "text", "text": "answer"}
            ]),
            "stop",
        )),
        Some(true),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let rendered = strip_lines(&component.render(80));
    assert_eq!(rendered.matches("Thinking...").count(), 1);
    assert!(rendered.contains("answer"));
}

#[test]
fn assistant_collapses_individual_thinking_runs_when_clicked() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(
            json!([
                {"type": "thinking", "thinking": "first reasoning"},
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "second reasoning"}
            ]),
            "stop",
        )),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let width = 80;
    let lines = component.render(width);
    let row = lines.iter().position(|l| strip(l).contains("first reasoning")).expect("thinking row");
    let handled = component.handle_mouse(&click(1, row as i64, width as i64, lines.len() as i64));
    assert!(handled.is_some_and(|r| r.handled));

    let collapsed = strip_lines(&component.render(width));
    assert!(!collapsed.contains("first reasoning"));
    assert!(collapsed.contains("Thinking..."));
    assert!(collapsed.contains("second reasoning"));
}

#[test]
fn assistant_uses_configured_output_padding() {
    let _theme = dark_theme();
    let component = AssistantMessageComponent::new(
        Some(assistant_message(
            json!([{"type": "text", "text": "hello"}, {"type": "thinking", "thinking": "reasoning"}]),
            "stop",
        )),
        Some(false),
        None,
        Some("Thinking..."),
        Some(1),
        None,
        None,
        None,
    );
    let lines: Vec<String> = component.render(80).iter().map(|l| strip(l)).collect();
    assert!(lines.iter().any(|l| l.contains(" hello")));
    assert!(lines.iter().any(|l| l.contains(" reasoning")));

    component.set_output_pad(0);
    let lines: Vec<String> = component.render(80).iter().map(|l| strip(l)).collect();
    assert!(lines.iter().any(|l| l.starts_with("hello")));
    assert!(lines.iter().any(|l| l.starts_with("reasoning")));
}

#[test]
fn assistant_chains_markdown_transformers_in_order() {
    let _theme = dark_theme();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (c1, c2) = (calls.clone(), calls.clone());
    let transformers = vec![
        transformer(move |markdown, context| {
            c1.lock().push("formula".into());
            assert_eq!(
                *context,
                MarkdownTransformContext {
                    message_type: MarkdownMessageType::Assistant,
                    is_streaming: false,
                    available_width: 78,
                    message_id: None,
                    timestamp: None,
                }
            );
            markdown.replace("$x^2$", "x²")
        }),
        transformer(move |markdown, _| {
            c2.lock().push("suffix".into());
            format!("{markdown} Done.")
        }),
    ];
    let component = AssistantMessageComponent::new(
        Some(assistant_message(json!([{"type": "text", "text": "The result is $x^2$."}]), "stop")),
        Some(false),
        None,
        Some("Thinking..."),
        Some(1),
        Some(&transformers),
        None,
        None,
    );
    assert!(strip_lines(&component.render(80)).contains("The result is x². Done."));
    assert_eq!(*calls.lock(), vec!["formula", "suffix"]);
}

#[test]
fn assistant_identifies_partial_markdown_as_streaming() {
    let _theme = dark_theme();
    let states = Arc::new(Mutex::new(Vec::<bool>::new()));
    let s = states.clone();
    let transformers = vec![transformer(move |markdown, context| {
        s.lock().push(context.is_streaming);
        if context.is_streaming { markdown.to_string() } else { format!("{markdown} transformed") }
    })];
    let message = assistant_message(json!([{"type": "text", "text": "partial"}]), "stop");
    let component =
        AssistantMessageComponent::new(None, Some(false), None, Some("Thinking..."), Some(1), Some(&transformers), None, None);

    component.update_content(message.clone(), Some(true));
    assert!(!strip_lines(&component.render(80)).contains("transformed"));

    component.update_content(message, Some(false));
    assert!(strip_lines(&component.render(80)).contains("partial transformed"));
    assert_eq!(*states.lock(), vec![true, false]);
}

#[test]
fn assistant_reapplies_transformers_when_width_changes() {
    let _theme = dark_theme();
    let widths = Arc::new(Mutex::new(Vec::<usize>::new()));
    let w = widths.clone();
    let transformers = vec![transformer(move |markdown, context| {
        w.lock().push(context.available_width);
        format!("{markdown} ({})", context.available_width)
    })];
    let component = AssistantMessageComponent::new(
        Some(assistant_message(json!([{"type": "text", "text": "answer"}]), "stop")),
        Some(false),
        None,
        Some("Thinking..."),
        Some(1),
        Some(&transformers),
        None,
        None,
    );
    assert!(strip_lines(&component.render(80)).contains("answer (78)"));
    component.render(80);
    assert!(strip_lines(&component.render(60)).contains("answer (58)"));
    assert_eq!(*widths.lock(), vec![78, 58]);
}

#[test]
fn assistant_continues_transformer_chain_after_failure() {
    let _theme = dark_theme();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (c1, c2, c3) = (calls.clone(), calls.clone(), calls.clone());
    let transformers = vec![
        transformer(move |markdown, _| {
            c1.lock().push("first".into());
            markdown.replace("still", "remains")
        }),
        transformer(move |_, _| {
            c2.lock().push("throw".into());
            panic!("broken transformer");
        }),
        transformer(move |markdown, _| {
            c3.lock().push("last".into());
            format!("{markdown} after error")
        }),
    ];
    let component = AssistantMessageComponent::new(
        Some(assistant_message(json!([{"type": "text", "text": "still visible"}]), "stop")),
        Some(false),
        None,
        Some("Thinking..."),
        Some(1),
        Some(&transformers),
        None,
        None,
    );
    assert!(strip_lines(&component.render(80)).contains("remains visible after error"));
    assert_eq!(*calls.lock(), vec!["first", "throw", "last"]);
}

#[test]
fn assistant_transforms_text_and_thinking_without_mutating_message() {
    let _theme = dark_theme();
    let message = assistant_message(
        json!([{"type": "text", "text": "answer"}, {"type": "thinking", "thinking": "reasoning"}]),
        "stop",
    );
    let original = message.clone();
    let transformers =
        vec![transformer(|markdown, context| format!("{}:{markdown}", serde_json::to_value(context.message_type).unwrap().as_str().unwrap()))];
    let component = AssistantMessageComponent::new(
        Some(message.clone()),
        Some(false),
        None,
        Some("Thinking..."),
        Some(1),
        Some(&transformers),
        None,
        None,
    );
    let rendered = strip_lines(&component.render(80));
    assert!(rendered.contains("assistant:answer"));
    assert!(rendered.contains("assistant-thinking:reasoning"));
    assert_eq!(message, original);
}

#[test]
fn user_message_uses_configured_output_padding() {
    let _theme = dark_theme();
    let padded = UserMessageComponent::new("hello", None, Some(1), None, None, None);
    assert!(padded.render(40).iter().map(|l| strip(l)).any(|l| l.starts_with(" hello")));
    let unpadded = UserMessageComponent::new("hello", None, Some(0), None, None, None);
    assert!(unpadded.render(40).iter().map(|l| strip(l)).any(|l| l.starts_with("hello")));
}

// ---------------------------------------------------------------------------
// user-message.test.ts
// ---------------------------------------------------------------------------

#[test]
fn user_message_keeps_height_while_moving_closing_markers() {
    let _theme = dark_theme();
    let component = UserMessageComponent::new("hello", None, None, None, None, None);
    let lines = component.render(20);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(OSC133_ZONE_START));
    assert!(lines[0].ends_with("\x1b[49m"));
    assert!(!lines[0].contains(OSC133_ZONE_END));
    assert!(lines[1].contains("hello"));
    assert!(lines[2].starts_with(&format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}")));
    assert!(lines[2].ends_with("\x1b[49m"));
}

#[test]
fn user_message_chains_transformers_with_user_context() {
    let _theme = dark_theme();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (c1, c2) = (calls.clone(), calls.clone());
    let transformers = vec![
        transformer(move |markdown, context| {
            c1.lock().push("formula".into());
            assert_eq!(
                *context,
                MarkdownTransformContext { message_type: MarkdownMessageType::User, is_streaming: false, available_width: 78, message_id: None, timestamp: None }
            );
            markdown.replace("$x^2$", "x²")
        }),
        transformer(move |markdown, _| {
            c2.lock().push("suffix".into());
            format!("{markdown} Done.")
        }),
    ];
    let component = UserMessageComponent::new("The input is $x^2$.", None, Some(1), Some(&transformers), None, None);
    assert!(strip_lines(&component.render(80)).contains("The input is x². Done."));
    assert_eq!(*calls.lock(), vec!["formula", "suffix"]);
}

#[test]
fn user_message_reapplies_transformers_when_invalidated() {
    let _theme = dark_theme();
    let suffix = Arc::new(Mutex::new("before".to_string()));
    let s = suffix.clone();
    let transformers = vec![transformer(move |markdown, _| format!("{markdown} {}", s.lock()))];
    let component = UserMessageComponent::new("Message", None, Some(1), Some(&transformers), None, None);
    assert!(strip_lines(&component.render(80)).contains("Message before"));
    *suffix.lock() = "after".into();
    component.invalidate();
    assert!(strip_lines(&component.render(80)).contains("Message after"));
}

// ---------------------------------------------------------------------------
// custom-message.test.ts
// ---------------------------------------------------------------------------

#[test]
fn custom_message_provides_output_padding_to_renderers() {
    let _theme = dark_theme();
    let seen = Arc::new(Mutex::new(Vec::<MessageRenderOptions>::new()));
    let s = seen.clone();
    let renderer: MessageRenderer = Arc::new(move |_message: &CustomMessage, options: &MessageRenderOptions, _theme: &Arc<Theme>| {
        s.lock().push(*options);
        Some(Arc::new(Text::new("custom", options.output_pad, 0, None)) as ComponentRef)
    });
    let message: CustomMessage = serde_json::from_value(json!({
        "role": "custom", "customType": "test", "content": "custom", "display": true, "timestamp": 0
    }))
    .unwrap();
    let component = CustomMessageComponent::new(&message, Some(&renderer), None, Some(1));
    {
        let seen = seen.lock();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].expanded);
        assert_eq!(seen[0].output_pad, 1);
    }
    assert!(component.render(40).iter().map(|l| strip(l)).any(|l| l.starts_with(" custom")));

    component.set_output_pad(0);
    assert_eq!(seen.lock().last().unwrap().output_pad, 0);
    assert!(component.render(40).iter().map(|l| strip(l)).any(|l| l.starts_with("custom")));
}

// ---------------------------------------------------------------------------
// collapsible-message-components.test.ts
// ---------------------------------------------------------------------------

const WIDTH: usize = 80;

fn click_row(component: &dyn Component, marker: &str) {
    let lines = component.render(WIDTH);
    let row = lines.iter().position(|l| strip(l).contains(marker)).expect("marker row");
    let result = component.handle_mouse(&click(2, row as i64, WIDTH as i64, lines.len() as i64));
    assert!(result.is_some_and(|r| r.handled));
}

#[test]
fn toggles_compaction_summary_when_clicked() {
    let _theme = dark_theme();
    let component = CompactionSummaryMessageComponent::new(
        &CompactionSummaryMessage {
            role: "compactionSummary".into(),
            summary: "compaction details".into(),
            tokens_before: 1234,
            timestamp: 0,
        },
        None,
        None,
    );
    assert!(!strip_lines(&component.render(WIDTH)).contains("compaction details"));
    assert!(strip_lines(&component.render(WIDTH)).contains("Compacted from 1,234 tokens"));
    click_row(&component, "[compaction]");
    assert!(strip_lines(&component.render(WIDTH)).contains("compaction details"));
    click_row(&component, "[compaction]");
    assert!(!strip_lines(&component.render(WIDTH)).contains("compaction details"));
}

#[test]
fn toggles_branch_summary_when_clicked() {
    let _theme = dark_theme();
    let component = BranchSummaryMessageComponent::new(
        &BranchSummaryMessage {
            role: "branchSummary".into(),
            summary: "branch details".into(),
            from_id: Some("entry-1".into()),
            timestamp: 0,
        },
        None,
        None,
    );
    assert!(!strip_lines(&component.render(WIDTH)).contains("branch details"));
    click_row(&component, "[branch]");
    assert!(strip_lines(&component.render(WIDTH)).contains("branch details"));
    click_row(&component, "[branch]");
    assert!(!strip_lines(&component.render(WIDTH)).contains("branch details"));
}

#[test]
fn toggles_skill_invocation_when_clicked() {
    let _theme = dark_theme();
    let component = SkillInvocationMessageComponent::new(
        &ParsedSkillBlock {
            name: "example-skill".into(),
            location: "/tmp/example-skill.md".into(),
            content: "skill details".into(),
            user_message: None,
        },
        None,
        None,
    );
    assert!(!strip_lines(&component.render(WIDTH)).contains("skill details"));
    click_row(&component, "[skill]");
    assert!(strip_lines(&component.render(WIDTH)).contains("skill details"));
    click_row(&component, "[skill]");
    assert!(!strip_lines(&component.render(WIDTH)).contains("skill details"));
}

// ---------------------------------------------------------------------------
// output-pad.test.ts
// ---------------------------------------------------------------------------

fn custom_tool_definition(name: &str) -> ToolDefinition {
    ToolDefinition::new(name, name, "custom tool", json!({}), |_id, _params, _signal, _update, _ctx| async {
        Ok(AgentToolResult::text("ok", Some(json!({}))))
    })
}

fn text_result(text: &str, is_error: bool) -> ToolExecutionComponentUpdateResultResult {
    ToolExecutionComponentUpdateResultResult {
        content: vec![ToolExecutionComponentUpdateResultResultContent {
            r#type: "text".into(),
            text: Some(text.into()),
            data: None,
            mime_type: None,
        }],
        details: None,
        is_error,
        duration_ms: None,
    }
}

/// Text lines without ANSI codes or trailing fill. Blank lines and full-width borders are skipped.
fn output_lines(component: &dyn Component) -> Vec<String> {
    let word = regex::Regex::new(r"[\w$(]").unwrap();
    component
        .render(60)
        .iter()
        .map(|l| strip(l).trim_end().to_string())
        .filter(|l| word.is_match(l))
        .collect()
}

fn assert_output_pad(make: impl Fn(i64) -> Box<dyn Fn(i64)>, render: impl Fn() -> Vec<String>) {
    let set_pad = make(0);
    let lines = render();
    assert!(lines.iter().all(|l| !l.starts_with(' ')), "{lines:?}");
    set_pad(1);
    let padded = render();
    assert_eq!(padded, lines.iter().map(|l| format!(" {l}")).collect::<Vec<_>>());
}

#[test]
fn output_pad_bash_execution() {
    let _theme = dark_theme();
    let (ui, _loop) = test_tui(80, 24);
    let component = Arc::new(BashExecutionComponent::new("pwd", ui, Some(false), Some(0)));
    component.append_output("/tmp");
    component.set_complete(Some(1), false, None, None);
    let c = component.clone();
    assert_output_pad(move |_| { let c = c.clone(); Box::new(move |p| c.set_output_pad(p)) }, || output_lines(&*component));
}

#[test]
fn output_pad_tool_execution() {
    let _theme = dark_theme();
    let (ui, _loop) = test_tui(80, 24);
    for definition in [Some(custom_tool_definition("custom_tool")), None] {
        let def = definition.map(ToolExecutionComponentToolDefinition::ToolDefinition);
        let component = Arc::new(ToolExecutionComponent::new(
            "custom_tool",
            "id",
            json!({}),
            Some(&ToolExecutionOptions { output_pad: Some(0), ..Default::default() }),
            def.as_ref(),
            ui.clone(),
            "/",
        ));
        component.update_result(&text_result("ok", false), None);
        let c = component.clone();
        assert_output_pad(move |_| { let c = c.clone(); Box::new(move |p| c.set_output_pad(p)) }, || output_lines(&*component));
    }
}

#[test]
fn output_pad_self_rendered_edit_result() {
    let _theme = dark_theme();
    let (ui, _loop) = test_tui(80, 24);
    let def = ToolExecutionComponentToolDefinition::ToolDefinition(crate::core::tools::edit::create_edit_tool_definition("/", None));
    let component = Arc::new(ToolExecutionComponent::new(
        "edit",
        "id",
        json!({"path": "file.txt", "edits": [{"oldText": "old", "newText": "new"}]}),
        Some(&ToolExecutionOptions { output_pad: Some(0), ..Default::default() }),
        Some(&def),
        ui,
        "/",
    ));
    component.update_result(&text_result("Could not find old text", true), None);
    let c = component.clone();
    assert_output_pad(move |_| { let c = c.clone(); Box::new(move |p| c.set_output_pad(p)) }, || output_lines(&*component));
}

#[test]
fn output_pad_compaction_summary() {
    let _theme = dark_theme();
    let component = Arc::new(CompactionSummaryMessageComponent::new(
        &CompactionSummaryMessage { role: "compactionSummary".into(), summary: "summary".into(), tokens_before: 10, timestamp: 0 },
        None,
        Some(0),
    ));
    let c = component.clone();
    assert_output_pad(move |_| { let c = c.clone(); Box::new(move |p| c.set_output_pad(p)) }, || output_lines(&*component));
}

// ---------------------------------------------------------------------------
// themed-text.test.ts
// ---------------------------------------------------------------------------

#[test]
fn themed_text_builds_lazily_and_rebuilds_after_invalidation() {
    let _theme = dark_theme();
    let builds = Arc::new(Mutex::new(0));
    let b = builds.clone();
    let text = ThemedText::new(
        Arc::new(move || {
            *b.lock() += 1;
            theme().fg(&ThemeColor::Accent, "hello")
        }),
        None,
        None,
    );
    assert_eq!(*builds.lock(), 0);
    let dark = text.render(20).join("");

    init_theme(Some("light"), Some(false));
    assert_eq!(text.render(20).join(""), dark);
    text.invalidate();
    assert!(text.render(20).join("").contains(&theme().get_fg_ansi(&ThemeColor::Accent)));
    assert_eq!(*builds.lock(), 2);
    init_theme(Some("dark"), Some(false));
}

// ---------------------------------------------------------------------------
// status-indicator.test.ts
// ---------------------------------------------------------------------------

#[test]
fn idle_status_matches_indicator_height() {
    let lines = IdleStatus::new().render(20);
    assert_eq!(lines, vec![" ".repeat(20), " ".repeat(20)]);
}

fn test_keybindings() -> Arc<KeybindingsManager> {
    Arc::new(KeybindingsManager::new(None, None))
}

#[test]
fn editor_top_border_unchanged_unless_opted_in() {
    let _theme = dark_theme();
    let (tui, _loop) = test_tui(80, 10);
    let editor = CustomEditor::new(tui.clone(), get_editor_theme(), test_keybindings(), None);
    let indicator = WorkingStatusIndicator::new(tui, "Working", None, None);
    editor.set_working_status_indicator(Some(indicator.indicator()));

    assert_eq!(strip(&editor.render(20)[0]), "─".repeat(20));
    let standalone = indicator.render(20)[1].clone();
    assert!(standalone.contains(&theme().get_fg_ansi(&ThemeColor::Accent)));
    assert!(standalone.contains(&theme().get_fg_ansi(&ThemeColor::Muted)));
    indicator.dispose();
}

#[test]
fn editor_embeds_working_indicator_when_opted_in() {
    let _theme = dark_theme();
    let (tui, _loop) = test_tui(80, 10);
    let editor = Arc::new(CustomEditor::new(
        tui.clone(),
        get_editor_theme(),
        test_keybindings(),
        Some(&CustomEditorOptions { embed_working_status: Some(true), ..Default::default() }),
    ));
    assert!(editor.embed_working_status());
    let thinking_high = theme().get_thinking_border_color(ThinkingLevel::High);
    editor.editor().set_border_color(Arc::new(move |s: &str| thinking_high(s.to_string())));
    let e = editor.clone();
    let indicator =
        WorkingStatusIndicator::new(tui, "Working", None, Some(Arc::new(move |text: String| (e.editor().border_color())(&text))));
    editor.set_working_status_indicator(Some(indicator.indicator()));

    let top_border = editor.render(20)[0].clone();
    assert_eq!(strip(&top_border), "── ⠋ Working ───────");
    assert_eq!(visible_width(&top_border), 20);
    assert_eq!(top_border.split(&theme().get_fg_ansi(&ThemeColor::ThinkingHigh)).count(), 5);
    indicator.dispose();
}

#[test]
fn editor_embeds_compaction_summary_and_retry_labels_within_width() {
    let _theme = dark_theme();
    let (tui, event_loop) = test_tui(80, 10);
    let editor = CustomEditor::new(
        tui.clone(),
        get_editor_theme(),
        test_keybindings(),
        Some(&CustomEditorOptions { embed_working_status: Some(true), ..Default::default() }),
    );
    let indicators: Vec<Arc<StatusIndicator>> = vec![
        CompactionStatusIndicator::new(tui.clone(), &CompactionStatusReason::Manual, None).indicator(),
        CompactionStatusIndicator::new(tui.clone(), &CompactionStatusReason::Threshold, None).indicator(),
        CompactionStatusIndicator::new(tui.clone(), &CompactionStatusReason::Overflow, None).indicator(),
        BranchSummaryStatusIndicator::new(tui.clone()).indicator(),
        RetryStatusIndicator::new(tui.clone(), 1, 3, 3000).indicator(),
    ];
    for indicator in &indicators {
        editor.set_working_status_indicator(Some(indicator.clone()));
        let label = strip(&indicator.render(120)[1]).trim().to_string();
        assert!(strip(&editor.render(120)[0]).contains(&format!("── {label} ")), "{label}");
        for width in [1, 4, 10, 20, 80, 120] {
            assert_eq!(visible_width(&editor.render(width)[0]), width, "{label} at {width}");
        }
    }
    event_loop.advance_ms(1000);
    assert!(strip(&editor.render(120)[0]).contains("Retrying (1/3) in 2s"));
    editor.set_working_status_indicator(None);
    assert_eq!(strip(&editor.render(120)[0]), "─".repeat(120));
    for indicator in indicators {
        indicator.dispose();
    }
}

#[test]
fn retry_countdown_is_disposed() {
    let _theme = dark_theme();
    let (tui, event_loop) = test_tui(80, 10);
    let indicator = RetryStatusIndicator::new(tui, 1, 3, 1000);
    indicator.dispose();
    let before = event_loop.timer_count();
    event_loop.advance_ms(2000);
    assert_eq!(before, 0, "no timers remain after dispose");
    assert!(strip(&indicator.render(120)[1]).contains("in 1s"));
}

// ---------------------------------------------------------------------------
// bash-execution-width.test.ts
// ---------------------------------------------------------------------------

#[test]
fn bash_collapsed_preview_respects_render_width() {
    let _theme = dark_theme();
    let (tui, _loop) = test_tui(200, 24);
    let component = BashExecutionComponent::new("pwd", tui, None, None);
    let long_line = "x".repeat(150);
    component.append_output(&format!("{long_line}\n{long_line}\n"));
    component.set_complete(Some(0), false, None, None);
    for line in component.render(80) {
        assert!(visible_width(&line) <= 80);
    }
}

#[test]
fn bash_recomputes_lines_when_width_changes() {
    let _theme = dark_theme();
    let (tui, _loop) = test_tui(200, 24);
    let component = BashExecutionComponent::new("echo hello", tui, None, None);
    component.append_output(&format!("{}\n", "abcdefghij".repeat(20)));
    component.set_complete(Some(0), false, None, None);
    for line in component.render(200) {
        assert!(visible_width(&line) <= 200);
    }
    for line in component.render(60) {
        assert!(visible_width(&line) <= 60);
    }
}

#[test]
fn bash_shows_exit_code_and_hidden_line_count() {
    let _theme = dark_theme();
    let (tui, _loop) = test_tui(80, 24);
    let component = BashExecutionComponent::new("seq 30", tui, None, None);
    let output: Vec<String> = (1..=30).map(|i| i.to_string()).collect();
    component.append_output(&output.join("\n"));
    component.set_complete(Some(2), false, None, None);
    let rendered = strip_lines(&component.render(80));
    assert!(rendered.contains("$ seq 30"));
    assert!(rendered.contains("... 10 more lines"));
    assert!(rendered.contains("(exit 2)"));
    assert!(!rendered.contains("\n 10\n"));
    assert_eq!(component.get_output(), output.join("\n"));
}

// ---------------------------------------------------------------------------
// footer-width.test.ts
// ---------------------------------------------------------------------------

struct FakeSession {
    session_name: String,
    model: Model,
    thinking_level: ThinkingLevel,
    entries: Mutex<Vec<SessionEntry>>,
    routed: Option<AgentSessionRoutedModel>,
    using_subscription: bool,
}

impl FooterSession for FakeSession {
    fn entries(&self) -> Vec<SessionEntry> {
        self.entries.lock().clone()
    }
    fn entry_count(&self) -> i64 {
        self.entries.lock().len() as i64
    }
    fn session_id(&self) -> String {
        "test-session".into()
    }
    fn leaf_id(&self) -> Option<String> {
        None
    }
    fn session_name(&self) -> Option<String> {
        Some(self.session_name.clone())
    }
    fn cwd(&self) -> String {
        "/tmp/project".into()
    }
    fn model(&self) -> Option<Model> {
        Some(self.model.clone())
    }
    fn thinking_level(&self) -> ThinkingLevel {
        self.thinking_level
    }
    fn routed_model(&self) -> Option<AgentSessionRoutedModel> {
        self.routed.clone()
    }
    fn context_usage(&self) -> Option<ContextUsage> {
        Some(ContextUsage { tokens: None, context_window: 200_000, percent: Some(12.3) })
    }
    fn is_using_subscription(&self, _provider: &str) -> bool {
        self.using_subscription
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Value {
    json!({
        "input": input, "output": output, "cacheRead": cache_read, "cacheWrite": cache_write,
        "totalTokens": input + output + cache_read + cache_write,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost}
    })
}

fn assistant_entry(usage: Value) -> SessionEntry {
    serde_json::from_value(json!({
        "type": "message", "id": "m", "parentId": null, "timestamp": "2026-01-01T00:00:00.000Z",
        "message": {
            "role": "assistant", "content": [], "api": "openai-responses", "provider": "test", "model": "m",
            "usage": usage, "stopReason": "stop", "timestamp": 0
        }
    }))
    .unwrap()
}

#[derive(Default)]
struct SessionOptions {
    session_name: String,
    model_id: Option<String>,
    provider: Option<String>,
    reasoning: bool,
    thinking_level: Option<ThinkingLevel>,
    entries: Vec<SessionEntry>,
    using_subscription: bool,
    routed: Option<(String, Option<ThinkingLevel>)>,
}

fn fake_session(options: SessionOptions) -> Arc<FakeSession> {
    let model = Model {
        id: options.model_id.unwrap_or_else(|| "test-model".into()),
        provider: options.provider.unwrap_or_else(|| "test".into()),
        context_window: 200_000,
        reasoning: options.reasoning,
        ..Default::default()
    };
    Arc::new(FakeSession {
        session_name: options.session_name,
        model,
        thinking_level: options.thinking_level.unwrap_or(ThinkingLevel::Off),
        entries: Mutex::new(options.entries),
        routed: options.routed.map(|(id, thinking_level)| AgentSessionRoutedModel {
            model: Model { id, ..Default::default() },
            thinking_level,
        }),
        using_subscription: options.using_subscription,
    })
}

fn footer_data(provider_count: i64) -> Arc<FooterDataProvider> {
    let provider = Arc::new(FooterDataProvider::new("/tmp/project-does-not-exist"));
    provider.set_available_provider_count(provider_count);
    provider
}

fn stats_line(footer: &FooterComponent, width: usize) -> String {
    strip(&footer.render(width)[1])
}

#[test]
fn format_cwd_for_footer_handles_home_prefixes() {
    assert_eq!(format_cwd_for_footer("/home/user2", Some("/home/user")), "/home/user2");
    assert_eq!(format_cwd_for_footer("/home/user", Some("/home/user")), "~");
    assert_eq!(format_cwd_for_footer("/home/user/project", Some("/home/user")), "~/project");
}

#[test]
fn footer_keeps_lines_within_width_for_wide_session_names() {
    let _theme = dark_theme();
    let session = fake_session(SessionOptions { session_name: "한글".repeat(30), ..Default::default() });
    let footer = FooterComponent::with_session(session, &footer_data(1));
    for line in footer.render(93) {
        assert!(visible_width(&line) <= 93);
    }
}

#[test]
fn footer_keeps_stats_within_width_for_wide_model_names() {
    let _theme = dark_theme();
    let session = fake_session(SessionOptions {
        model_id: Some("模".repeat(30)),
        provider: Some("공급자".into()),
        reasoning: true,
        thinking_level: Some(ThinkingLevel::High),
        entries: vec![assistant_entry(usage(12_345, 6_789, 0, 0, 1.234))],
        ..Default::default()
    });
    let footer = FooterComponent::with_session(session, &footer_data(2));
    for line in footer.render(60) {
        assert!(visible_width(&line) <= 60);
    }
}

#[test]
fn footer_shows_routed_physical_model() {
    let _theme = dark_theme();
    let session = fake_session(SessionOptions {
        model_id: Some("auto".into()),
        reasoning: true,
        thinking_level: Some(ThinkingLevel::High),
        routed: Some(("gpt-5.6-luna".into(), Some(ThinkingLevel::Medium))),
        ..Default::default()
    });
    let footer = FooterComponent::with_session(session, &footer_data(1));
    assert!(stats_line(&footer, 120).contains("auto • high → gpt-5.6-luna • medium"));
}

#[test]
fn footer_includes_summary_and_tool_result_usage_in_cost() {
    let _theme = dark_theme();
    let branch: SessionEntry = serde_json::from_value(json!({
        "type": "branch_summary", "id": "b", "parentId": null, "timestamp": "t", "fromId": "x", "summary": "s",
        "usage": usage(20, 5, 0, 0, 0.25)
    }))
    .unwrap();
    let compaction: SessionEntry = serde_json::from_value(json!({
        "type": "compaction", "id": "c", "parentId": null, "timestamp": "t", "summary": "s",
        "firstKeptEntryId": "x", "tokensBefore": 1, "usage": usage(5, 2, 0, 0, 0.125)
    }))
    .unwrap();
    let tool: SessionEntry = serde_json::from_value(json!({
        "type": "message", "id": "t", "parentId": null, "timestamp": "t",
        "message": {
            "role": "toolResult", "toolCallId": "1", "toolName": "x", "content": [], "isError": false,
            "timestamp": 0, "usage": usage(15, 3, 0, 0, 0.375)
        }
    }))
    .unwrap();
    let session = fake_session(SessionOptions {
        entries: vec![assistant_entry(usage(100, 10, 0, 0, 0.5)), branch, compaction, tool],
        ..Default::default()
    });
    let footer = FooterComponent::with_session(session, &footer_data(1));
    assert!(stats_line(&footer, 120).contains("$1.250"), "{}", stats_line(&footer, 120));
}

#[test]
fn footer_updates_cached_totals_after_append() {
    let _theme = dark_theme();
    let session = fake_session(SessionOptions {
        entries: vec![assistant_entry(usage(10, 1, 0, 0, 0.5))],
        ..Default::default()
    });
    let footer = FooterComponent::with_session(session.clone(), &footer_data(1));
    assert!(stats_line(&footer, 120).contains("$0.500"));
    session.entries.lock().push(assistant_entry(usage(10, 1, 0, 0, 0.5)));
    assert!(stats_line(&footer, 120).contains("$1.000"));
}

#[test]
fn footer_shows_latest_cache_hit_rate() {
    let _theme = dark_theme();
    let session = fake_session(SessionOptions {
        entries: vec![assistant_entry(usage(100, 10, 50, 50, 0.001))],
        ..Default::default()
    });
    let footer = FooterComponent::with_session(session, &footer_data(1));
    assert!(stats_line(&footer, 120).contains("CH25.0%"));
}

#[test]
fn footer_marks_subscription_costs() {
    let _theme = dark_theme();
    let kimi = fake_session(SessionOptions {
        provider: Some("kimi-coding".into()),
        entries: vec![assistant_entry(usage(100, 10, 0, 0, 1.234))],
        ..Default::default()
    });
    assert!(stats_line(&FooterComponent::with_session(kimi, &footer_data(1)), 120).contains("$1.234 (sub)"));

    let anthropic = fake_session(SessionOptions {
        provider: Some("anthropic".into()),
        using_subscription: true,
        ..Default::default()
    });
    assert!(stats_line(&FooterComponent::with_session(anthropic, &footer_data(1)), 120).contains("$0.000 (sub)"));

    let openrouter = fake_session(SessionOptions {
        provider: Some("openrouter".into()),
        entries: vec![assistant_entry(usage(100, 10, 0, 0, 1.234))],
        ..Default::default()
    });
    let stats = stats_line(&FooterComponent::with_session(openrouter, &footer_data(1)), 120);
    assert!(stats.contains("$1.234"));
    assert!(!stats.contains("(sub)"));
}

// ---------------------------------------------------------------------------
// mermaid.test.ts
// ---------------------------------------------------------------------------

fn transform_mermaid(
    markdown: &str,
    max_width: usize,
    is_streaming: bool,
    message_type: MarkdownMessageType,
    mode: MermaidRenderingMode,
    theme: Option<Arc<Theme>>,
) -> String {
    let transformer =
        create_mermaid_markdown_transformer(&MermaidTransformerOptions { get_mode: Arc::new(move || mode), theme });
    transformer(markdown, &MarkdownTransformContext { message_type, is_streaming, available_width: max_width, message_id: None, timestamp: None })
}

fn mermaid(markdown: &str) -> String {
    transform_mermaid(markdown, 100, false, MarkdownMessageType::Assistant, MermaidRenderingMode::Streaming, None)
}

#[test]
fn mermaid_replaces_code_blocks_with_diagrams() {
    let rendered = mermaid("Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```\nAfter");
    assert!(rendered.contains("Before"));
    assert!(rendered.contains("┌───────┐"));
    assert!(rendered.contains("│ Start ├───▶│ Done │"));
    assert!(rendered.contains("└───────┘    └──────┘`\nAfter"));
    assert!(!rendered.contains("```mermaid"));
}

#[test]
fn mermaid_leaves_unsupported_and_oversized_diagrams() {
    let unsupported = "```mermaid\npie\n  title Pets\n  \"Dogs\" : 4\n```";
    let oversized = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
    assert_eq!(mermaid(unsupported), unsupported);
    assert_eq!(
        transform_mermaid(oversized, 10, false, MarkdownMessageType::Assistant, MermaidRenderingMode::Streaming, None),
        oversized
    );
}

#[test]
fn mermaid_maps_semantic_spans_through_theme() {
    let theme = get_theme_by_name("dark").expect("dark theme");
    let rendered = transform_mermaid(
        "```mermaid\nflowchart LR\n  A --> B\n```",
        100,
        false,
        MarkdownMessageType::Assistant,
        MermaidRenderingMode::Streaming,
        Some(theme.clone()),
    );
    assert!(rendered.contains(&theme.get_fg_ansi(&ThemeColor::BorderMuted)));
    assert!(rendered.contains(&theme.get_fg_ansi(&ThemeColor::Accent)));
}

#[test]
fn mermaid_renders_incomplete_blocks_while_streaming() {
    let rendered = transform_mermaid(
        "```mermaid\nflowchart LR\n  A --> B",
        100,
        true,
        MarkdownMessageType::Assistant,
        MermaidRenderingMode::Streaming,
        None,
    );
    assert!(rendered.contains("───▶"));
}

#[test]
fn mermaid_renders_class_assignments() {
    let rendered = mermaid("```mermaid\nflowchart LR\n  A[Foo]:::highlight --> B[Bar]\n```");
    assert!(!rendered.contains("```mermaid"));
    assert!(!rendered.contains("Mermaid diagram not rendered"));
    assert!(rendered.contains("│ Foo ├───▶│ Bar │"));
}

#[test]
fn mermaid_falls_back_with_warning_after_streaming() {
    let markdown = "```mermaid\nflowchart LR\n  A[Foo] invalid\n```";
    let final_ = mermaid(markdown);
    let followed = mermaid(&format!("{markdown}\nFollowing text"));
    let streaming =
        transform_mermaid(markdown, 100, true, MarkdownMessageType::Assistant, MermaidRenderingMode::Streaming, None);
    assert!(final_.contains(markdown));
    assert!(final_.contains("```\n`Mermaid diagram not rendered"));
    assert!(final_.contains("dropped, expected a link: \"invalid\""));
    assert!(!final_.contains("more)"));
    assert!(followed.contains("  \nFollowing text"));
    assert!(!streaming.contains("Mermaid diagram not rendered"));
    assert!(!streaming.contains("```mermaid"));
    assert!(streaming.contains("│ Foo │"));
}

#[test]
fn mermaid_summarizes_additional_warnings() {
    let markdown = "```mermaid\nflowchart LR\n  A[Foo] invalid\n  B[Bar] also-invalid\n```";
    let rendered = mermaid(markdown);
    assert!(rendered.contains(markdown));
    assert!(rendered.contains("dropped, expected a link: \"invalid\""));
    assert!(rendered.contains("(+1 more)"));
    assert!(!rendered.contains("dropped, expected a link: \"also-invalid\""));
}

#[test]
fn mermaid_respects_modes_and_skips_thinking() {
    let markdown = "```mermaid\nflowchart LR\n  A --> B\n```";
    let t = |streaming, message_type, mode| transform_mermaid(markdown, 100, streaming, message_type, mode, None);
    assert_eq!(t(false, MarkdownMessageType::Assistant, MermaidRenderingMode::Off), markdown);
    assert_eq!(t(true, MarkdownMessageType::Assistant, MermaidRenderingMode::Final), markdown);
    assert!(!t(false, MarkdownMessageType::Assistant, MermaidRenderingMode::Final).contains("```mermaid"));
    assert_eq!(t(false, MarkdownMessageType::AssistantThinking, MermaidRenderingMode::Streaming), markdown);
}

// ---------------------------------------------------------------------------
// tool-execution-component.test.ts
// ---------------------------------------------------------------------------

fn tool_component(
    name: &str,
    args: Value,
    definition: Option<ToolExecutionComponentToolDefinition>,
) -> ToolExecutionComponent {
    let (ui, _loop) = test_tui(120, 24);
    ToolExecutionComponent::new(name, "tool-1", args, None, definition.as_ref(), ui, &crate::utils::paths::current_dir_string())
}

fn renderers(call: Option<&'static str>, result: Option<&'static str>, shell: Option<RenderShell>) -> ToolExecutionComponentToolDefinition {
    let mut def = custom_tool_definition("custom_tool");
    def.render_shell = shell;
    def.render_call = call.map(|text| {
        Arc::new(move |_args: &Value, _theme: &Arc<Theme>, _ctx: &crate::core::extensions::types::ToolRenderContext| {
            Arc::new(Text::new(text, 0, 0, None)) as ComponentRef
        }) as crate::core::extensions::types::ToolRenderCallFn
    });
    def.render_result = result.map(|text| {
        Arc::new(
            move |_r: &AgentToolResult,
                  _o: crate::core::extensions::types::ToolRenderResultOptions,
                  _theme: &Arc<Theme>,
                  _ctx: &crate::core::extensions::types::ToolRenderContext| {
                Arc::new(Text::new(text, 0, 0, None)) as ComponentRef
            },
        ) as crate::core::extensions::types::ToolRenderResultFn
    });
    ToolExecutionComponentToolDefinition::ToolDefinition(def)
}

#[test]
fn tool_stacks_custom_call_and_result_renderers() {
    let _theme = dark_theme();
    let component = tool_component("custom_tool", json!({}), Some(renderers(Some("custom call"), Some("custom result"), None)));
    assert!(strip_lines(&component.render(120)).contains("custom call"));
    component.update_result(&text_result("done", false), Some(false));
    let rendered = strip_lines(&component.render(120));
    assert!(rendered.contains("custom call"));
    assert!(rendered.contains("custom result"));
}

#[test]
fn tool_self_rendered_empty_rows_take_no_space() {
    let _theme = dark_theme();
    let component =
        tool_component("custom_tool", json!({}), Some(renderers(Some(""), Some(""), Some(RenderShell::SelfRendered))));
    assert!(component.render(120).is_empty());
    component.update_result(
        &ToolExecutionComponentUpdateResultResult { content: vec![], details: Some(json!({})), is_error: false, duration_ms: None },
        Some(false),
    );
    assert!(component.render(120).is_empty());
}

#[test]
fn tool_shares_renderer_state_between_slots() {
    let _theme = dark_theme();
    let mut def = custom_tool_definition("custom_tool");
    def.render_call = Some(Arc::new(|_args: &Value, _theme: &Arc<Theme>, ctx: &crate::core::extensions::types::ToolRenderContext| {
        let token = crate::core::tools::render_utils::with_render_state(&ctx.state, |s: &mut Option<String>| {
            s.get_or_insert_with(|| "shared-token".to_string()).clone()
        });
        Arc::new(Text::new(format!("custom call {token}"), 0, 0, None)) as ComponentRef
    }));
    def.render_result = Some(Arc::new(
        |_r: &AgentToolResult,
         _o: crate::core::extensions::types::ToolRenderResultOptions,
         _theme: &Arc<Theme>,
         ctx: &crate::core::extensions::types::ToolRenderContext| {
            let token = crate::core::tools::render_utils::with_render_state(&ctx.state, |s: &mut Option<String>| {
                s.clone().unwrap_or_default()
            });
            Arc::new(Text::new(format!("custom result {token}"), 0, 0, None)) as ComponentRef
        },
    ));
    let component = tool_component("custom_tool", json!({}), Some(ToolExecutionComponentToolDefinition::ToolDefinition(def)));
    component.update_result(&text_result("done", false), Some(false));
    let rendered = strip_lines(&component.render(120));
    assert!(rendered.contains("custom call shared-token"));
    assert!(rendered.contains("custom result shared-token"));
}

#[test]
fn tool_exposes_args_in_result_context() {
    let _theme = dark_theme();
    let mut def = custom_tool_definition("custom_tool");
    def.render_call = Some(Arc::new(|_a: &Value, _t: &Arc<Theme>, _c: &crate::core::extensions::types::ToolRenderContext| {
        Arc::new(Text::new("call", 0, 0, None)) as ComponentRef
    }));
    def.render_result = Some(Arc::new(
        |_r: &AgentToolResult,
         _o: crate::core::extensions::types::ToolRenderResultOptions,
         _t: &Arc<Theme>,
         ctx: &crate::core::extensions::types::ToolRenderContext| {
            Arc::new(Text::new(format!("arg:{}", ctx.args["foo"].as_str().unwrap_or("")), 0, 0, None)) as ComponentRef
        },
    ));
    let component =
        tool_component("custom_tool", json!({"foo": "bar"}), Some(ToolExecutionComponentToolDefinition::ToolDefinition(def)));
    component.update_result(&text_result("done", false), Some(false));
    assert!(strip_lines(&component.render(120)).contains("arg:bar"));
}

#[test]
fn tool_shows_arguments_in_fallback_call_header() {
    let _theme = dark_theme();
    let long_value = "x".repeat(200);
    let component = tool_component(
        "custom_tool",
        json!({"query": "pi", "long": long_value, "text": "line one\nline two"}),
        Some(ToolExecutionComponentToolDefinition::ToolDefinition(custom_tool_definition("custom_tool"))),
    );
    let collapsed = strip_lines(&component.render(300));
    assert!(collapsed.contains("custom_tool query=\"pi\" long=\"xxx"), "{collapsed}");
    assert!(collapsed.contains("..."));
    assert!(!collapsed.contains(&long_value));

    component.set_expanded(true);
    let expanded = strip_lines(&component.render(300));
    assert!(expanded.contains("  query: pi"));
    assert!(expanded.contains(&long_value));
    let lines: Vec<&str> = expanded.split('\n').map(str::trim_end).collect();
    let text_line = lines.iter().position(|l| l.ends_with("  text: line one")).expect("text line");
    assert!(regex::Regex::new(r"^\s+ {4}line two$").unwrap().is_match(lines[text_line + 1]));
}

#[test]
fn tool_collapses_fallback_results_until_expanded() {
    let _theme = dark_theme();
    let component = tool_component(
        "custom_tool",
        json!({"foo": "bar"}),
        Some(ToolExecutionComponentToolDefinition::ToolDefinition(custom_tool_definition("custom_tool"))),
    );
    let output: Vec<String> = (1..=15).map(|i| format!("line-{i}")).collect();
    component.update_result(&text_result(&output.join("\n"), false), Some(false));
    let collapsed = strip_lines(&component.render(120));
    assert!(collapsed.contains("custom_tool"));
    assert!(collapsed.contains("line-10"));
    assert!(!collapsed.contains("line-11"));
    assert!(collapsed.contains("5 more lines"));
    assert!(collapsed.contains("to expand"));

    component.set_expanded(true);
    let expanded = strip_lines(&component.render(120));
    assert!(expanded.contains("line-15"));
    assert!(!expanded.contains("more lines"));
}

#[test]
fn tool_without_definition_shows_name_and_args() {
    let _theme = dark_theme();
    let component = tool_component("read", json!({"file_path": "README.md"}), None);
    let rendered = strip_lines(&component.render(120));
    assert!(rendered.contains("read"));
    assert!(rendered.contains("README.md"));
}

#[test]
fn tool_read_results_collapse_until_expanded_and_expand_on_click() {
    let _theme = dark_theme();
    let cwd = crate::utils::paths::current_dir_string();
    let def = crate::core::tools::read::create_read_tool_definition(&cwd, None);
    let component = tool_component(
        "read",
        json!({"path": "notes.txt"}),
        Some(ToolExecutionComponentToolDefinition::ToolDefinition(def)),
    );
    component.update_result(&text_result("hidden content", false), Some(false));
    let width = 120;
    let lines = component.render(width);
    let collapsed = strip_lines(&lines);
    assert!(collapsed.contains("read"));
    assert!(collapsed.contains("notes.txt"));
    assert!(!collapsed.contains("hidden content"));

    let row = lines.iter().position(|l| strip(l).contains("notes.txt")).expect("result row");
    let result = component.handle_mouse(&click(2, row as i64, width as i64, lines.len() as i64));
    assert!(result.is_some_and(|r| r.handled));
    assert!(strip_lines(&component.render(width)).contains("hidden content"));
}

#[test]
fn tool_uses_tool_renderers_variant() {
    let _theme = dark_theme();
    let definition = ToolExecutionComponentToolDefinition::ToolRenderers(ToolRenderers {
        render_shell: None,
        render_call: None,
        render_result: None,
    });
    let component = tool_component("custom_tool", json!({"a": 1}), Some(definition));
    component.update_result(&text_result("plain output", false), Some(false));
    let rendered = strip_lines(&component.render(120));
    assert!(rendered.contains("custom_tool"));
    assert!(rendered.contains("plain output"));
}

// ---------------------------------------------------------------------------
// custom-editor-history-keybindings.test.ts
// ---------------------------------------------------------------------------

#[test]
fn custom_editor_history_binding_wins_over_model_cycling() {
    let _theme = dark_theme();
    let keybindings = Arc::new(KeybindingsManager::new(
        Some(vec![
            ("tui.editor.historyPrevious".to_string(), Some(pi_tui::keybindings::KeySpec::from("ctrl+p"))),
            ("tui.editor.historyNext".to_string(), Some(pi_tui::keybindings::KeySpec::from("ctrl+n"))),
        ]),
        None,
    ));
    let previous = pi_tui::keybindings::get_keybindings();
    pi_tui::keybindings::set_keybindings(keybindings.tui_manager());
    let (tui, _loop) = test_tui(80, 24);
    let editor = CustomEditor::new(tui, get_editor_theme(), keybindings, None);
    let cycles = Arc::new(Mutex::new(0));
    let c = cycles.clone();
    editor.on_action(&"app.model.cycleForward".to_string(), Arc::new(move || *c.lock() += 1));
    EditorComponent::add_to_history(&editor, "previous prompt");
    EditorComponent::set_text(&editor, "draft");

    editor.handle_input("\x10"); // Ctrl+P
    assert_eq!(EditorComponent::get_text(&editor), "previous prompt");
    assert_eq!(*cycles.lock(), 0);

    editor.handle_input("\x0e"); // Ctrl+N
    assert_eq!(EditorComponent::get_text(&editor), "draft");
    pi_tui::keybindings::set_keybindings(previous);
}

#[test]
fn markdown_transform_context_persisted_identity_wire_roundtrip() {
    let persisted = json!({
        "messageType": "user", "isStreaming": false, "availableWidth": 78,
        "messageId": "entry-abc", "timestamp": "2025-01-15T10:30:00.000Z"
    });
    let context: MarkdownTransformContext = serde_json::from_value(persisted.clone()).unwrap();
    assert_eq!(serde_json::to_value(context).unwrap(), persisted);
    let live = json!({"messageType": "assistant", "isStreaming": true, "availableWidth": 78});
    let context: MarkdownTransformContext = serde_json::from_value(live.clone()).unwrap();
    assert_eq!(serde_json::to_value(context).unwrap(), live);
}

#[test]
fn markdown_transformers_receive_persisted_identity_and_absent_live_metadata() {
    let _theme = dark_theme();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let transformers = vec![transformer(move |markdown, context| {
        captured.lock().push(context.clone());
        markdown.to_string()
    })];
    let timestamp = "2025-01-15T10:30:00.000Z";
    let user = UserMessageComponent::new(
        "restored", None, Some(1), Some(&transformers), Some("entry-user"), Some(timestamp),
    );
    user.render(80);
    user.set_output_pad(0);
    user.render(60);
    UserMessageComponent::new("live", None, Some(1), Some(&transformers), None, None).render(80);
    let message = assistant_message(
        json!([{"type": "text", "text": "answer"}, {"type": "thinking", "thinking": "reasoning"}]),
        "stop",
    );
    let assistant = AssistantMessageComponent::new(
        Some(message.clone()), Some(false), None, None, Some(1), Some(&transformers),
        Some("entry-assistant"), Some(timestamp),
    );
    assistant.render(80);
    assistant.invalidate();
    assistant.render(60);
    let live = AssistantMessageComponent::new(
        None, Some(false), None, None, Some(1), Some(&transformers), None, None,
    );
    live.update_content(message, Some(true));
    live.render(80);
    let seen = seen.lock();
    assert_eq!(seen.len(), 9);
    for index in [0, 1] {
        assert_eq!(seen[index].message_type, MarkdownMessageType::User);
        assert_eq!(seen[index].message_id.as_deref(), Some("entry-user"));
        assert_eq!(seen[index].timestamp.as_deref(), Some(timestamp));
    }
    assert_eq!(seen[0].available_width, 78);
    assert_eq!(seen[1].available_width, 60);
    for index in [3, 4, 5, 6] {
        assert_eq!(seen[index].message_id.as_deref(), Some("entry-assistant"));
        assert_eq!(seen[index].timestamp.as_deref(), Some(timestamp));
        assert!(!seen[index].is_streaming);
    }
    assert_eq!(seen[3].message_type, MarkdownMessageType::Assistant);
    assert_eq!(seen[4].message_type, MarkdownMessageType::AssistantThinking);
    for index in [2, 7, 8] {
        assert_eq!(seen[index].message_id, None);
        assert_eq!(seen[index].timestamp, None);
        assert_eq!(seen[index].is_streaming, index != 2);
    }
}
