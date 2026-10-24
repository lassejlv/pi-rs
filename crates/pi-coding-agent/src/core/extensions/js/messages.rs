//! pi's interactive message components (`AssistantMessageComponent`, `ToolExecutionComponent`,
//! ...) constructed by JavaScript extensions, e.g. to show a subagent's transcript in a custom
//! UI. They are the Rust components behind `RustComponent` proxies (`pi-coding-agent.js`).

use std::sync::Arc;

use pi_tui::tui::{ComponentRef, Tui, downcast_component};
use serde_json::{Value, json};

use super::Bridge;
use super::api::{arg, opt, str_arg};
use crate::core::extensions::types::{MarkdownTransformContext, MarkdownTransformer, ToolRenderers};
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use crate::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use crate::modes::interactive::components::custom_message::CustomMessageComponent;
use crate::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use crate::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionComponentToolDefinition, ToolExecutionOptions,
};
use crate::modes::interactive::components::user_message::UserMessageComponent;

fn markdown_transformers(bridge: &Bridge, value: &Value) -> Vec<MarkdownTransformer> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| bridge.handle(v))
        .map(|h| {
            Arc::new(move |markdown: &str, ctx: &MarkdownTransformContext| {
                match h.call_blocking(vec![json!(markdown), serde_json::to_value(ctx).unwrap_or(Value::Null)]) {
                    Ok(Value::String(s)) => s,
                    _ => markdown.to_string(),
                }
            }) as MarkdownTransformer
        })
        .collect()
}

/// The TUI a component was given: a TUI proxy, otherwise the TUI extensions last saw (a
/// stand-in `{ requestRender() {} }` object gets pi's TUI), otherwise a detached one.
fn tui_of(bridge: &Bridge, value: &Value) -> Arc<Tui> {
    value["$tui"]
        .as_u64()
        .and_then(|id| bridge.components.tuis.get(id))
        .or_else(|| bridge.components.tuis.latest())
        .unwrap_or_else(|| Tui::main_screen(Arc::new(pi_tui::testing::VirtualTerminal::new(80, 24))))
}

fn pad(value: &Value) -> Option<i64> {
    value.as_f64().map(|v| v as i64)
}

/// `new <Component>(...)` for the message components (`rc.new` kinds).
pub(crate) fn new_component(bridge: &Arc<Bridge>, kind: &str, payload: &Value) -> anyhow::Result<Option<ComponentRef>> {
    let a = |i: usize| arg(payload, i);
    let theme = |i: usize| super::component::markdown_theme_ref(bridge, &a(i));
    Ok(Some(match kind {
        "assistantMessage" => {
            let message = opt(&a(0))?;
            let transformers = markdown_transformers(bridge, &a(5));
            Arc::new(AssistantMessageComponent::new(
                message,
                a(1).as_bool(),
                theme(2).as_ref(),
                a(3).as_str(),
                pad(&a(4)),
                Some(&transformers),
                a(6).as_str(),
                a(7).as_str(),
            ))
        }
        "userMessage" => {
            let transformers = markdown_transformers(bridge, &a(3));
            Arc::new(UserMessageComponent::new(
                &str_arg(&json!({ "t": a(0) }), "t"),
                theme(1).as_ref(),
                pad(&a(2)),
                Some(&transformers),
                a(4).as_str(),
                a(5).as_str(),
            ))
        }
        "toolExecution" => {
            let options: Option<ToolExecutionOptions> = opt(&a(3))?;
            let d = a(4);
            let definition = if d.is_object() {
                let (render_call, render_result) = super::component::tool_renderers(bridge, &d["renderCall"], &d["renderResult"])?;
                Some(ToolExecutionComponentToolDefinition::ToolRenderers(ToolRenderers {
                    render_shell: opt(&d["renderShell"]).unwrap_or(None),
                    render_call,
                    render_result,
                }))
            } else {
                None
            };
            Arc::new(ToolExecutionComponent::new(
                a(0).as_str().unwrap_or_default(),
                a(1).as_str().unwrap_or_default(),
                a(2),
                options.as_ref(),
                definition.as_ref(),
                tui_of(bridge, &a(5)),
                a(6).as_str().unwrap_or(&bridge.cwd),
            ))
        }
        "compactionSummary" => {
            Arc::new(CompactionSummaryMessageComponent::new(&serde_json::from_value(a(0))?, theme(1).as_ref(), pad(&a(2))))
        }
        "branchSummary" => Arc::new(BranchSummaryMessageComponent::new(&serde_json::from_value(a(0))?, theme(1).as_ref(), pad(&a(2)))),
        "skillInvocation" => {
            Arc::new(SkillInvocationMessageComponent::new(&serde_json::from_value(a(0))?, theme(1).as_ref(), pad(&a(2))))
        }
        "customMessage" => {
            let renderer = bridge.handle(&a(1)).map(|h| super::component::js_message_renderer(bridge.clone(), h));
            Arc::new(CustomMessageComponent::new(&serde_json::from_value(a(0))?, renderer.as_ref(), theme(2).as_ref(), pad(&a(3))))
        }
        "bashExecution" => Arc::new(BashExecutionComponent::new(
            a(0).as_str().unwrap_or_default(),
            tui_of(bridge, &a(1)),
            a(2).as_bool(),
            pad(&a(3)),
        )),
        _ => return Ok(None),
    }))
}

/// Methods of the message components (`rc` calls on a `RustComp::Other`).
pub(crate) fn call(component: &ComponentRef, method: &str, payload: &Value) -> anyhow::Result<Option<Value>> {
    let a = |i: usize| arg(payload, i);
    let unknown = || Ok(None);
    if let Some(c) = downcast_component::<AssistantMessageComponent>(component) {
        match method {
            "updateContent" => c.update_content(serde_json::from_value(a(0))?, a(1).as_bool()),
            "setHideThinkingBlock" => c.set_hide_thinking_block(a(0).as_bool().unwrap_or(false)),
            "setHiddenThinkingLabel" => c.set_hidden_thinking_label(a(0).as_str().unwrap_or_default()),
            "setOutputPad" => c.set_output_pad(pad(&a(0)).unwrap_or(1)),
            _ => return unknown(),
        }
        return Ok(Some(Value::Null));
    }
    if let Some(c) = downcast_component::<UserMessageComponent>(component) {
        return match method {
            "setOutputPad" => {
                c.set_output_pad(pad(&a(0)).unwrap_or(1));
                Ok(Some(Value::Null))
            }
            _ => unknown(),
        };
    }
    if let Some(c) = downcast_component::<ToolExecutionComponent>(component) {
        match method {
            "updateArgs" => c.update_args(a(0)),
            "markExecutionStarted" => c.mark_execution_started(),
            "setArgsComplete" => c.set_args_complete(),
            "updateResult" => {
                let mut result = a(0);
                if result.get("isError").is_none() {
                    result["isError"] = json!(false);
                }
                c.update_result(&serde_json::from_value(result)?, a(1).as_bool());
            }
            "setExpanded" => c.set_expanded(a(0).as_bool().unwrap_or(false)),
            "setOutputPad" => c.set_output_pad(pad(&a(0)).unwrap_or(1)),
            "setShowImages" => c.set_show_images(a(0).as_bool().unwrap_or(true)),
            "setImageWidthCells" => c.set_image_width_cells(pad(&a(0)).unwrap_or(60)),
            _ => return unknown(),
        }
        return Ok(Some(Value::Null));
    }
    if let Some(c) = downcast_component::<BashExecutionComponent>(component) {
        match method {
            "setExpanded" => c.set_expanded(a(0).as_bool().unwrap_or(false)),
            "setOutputPad" => c.set_output_pad(pad(&a(0)).unwrap_or(1)),
            "appendOutput" => c.append_output(a(0).as_str().unwrap_or_default()),
            "setComplete" => c.set_complete(
                a(0).as_f64().map(|v| v as i64),
                a(1).as_bool().unwrap_or(false),
                opt(&a(2))?.as_ref(),
                a(3).as_str(),
            ),
            "getOutput" => return Ok(Some(json!(c.get_output()))),
            "getCommand" => return Ok(Some(json!(c.get_command()))),
            _ => return unknown(),
        }
        return Ok(Some(Value::Null));
    }
    macro_rules! expandable {
        ($ty:ty) => {
            if let Some(c) = downcast_component::<$ty>(component) {
                match method {
                    "setExpanded" => c.set_expanded(a(0).as_bool().unwrap_or(false)),
                    "setOutputPad" => c.set_output_pad(pad(&a(0)).unwrap_or(1)),
                    _ => return unknown(),
                }
                return Ok(Some(Value::Null));
            }
        };
    }
    expandable!(CompactionSummaryMessageComponent);
    expandable!(BranchSummaryMessageComponent);
    expandable!(SkillInvocationMessageComponent);
    expandable!(CustomMessageComponent);
    unknown()
}
