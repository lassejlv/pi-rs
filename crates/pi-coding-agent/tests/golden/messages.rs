//! Builders for `messages.json` (scripts/golden/gen-messages.mjs).

use pi_coding_agent::core::agent_session::ParsedSkillBlock;
use pi_coding_agent::core::messages::{BranchSummaryMessage, CompactionSummaryMessage, CustomMessage};
use pi_coding_agent::core::tools::truncate::TruncationResult;
use pi_coding_agent::modes::interactive::components::assistant_message::AssistantMessageComponent;
use pi_coding_agent::modes::interactive::components::bash_execution::BashExecutionComponent;
use pi_coding_agent::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use pi_coding_agent::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use pi_coding_agent::modes::interactive::components::custom_message::CustomMessageComponent;
use pi_coding_agent::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use pi_coding_agent::modes::interactive::components::user_message::UserMessageComponent;
use serde_json::Value;

use super::harness::*;

pub fn build(kind: &str, input: &Value) -> Option<Built> {
    Some(match kind {
        "assistant_message" => {
            let c = AssistantMessageComponent::new(
                None,
                Some(b(input, "hideThinking")),
                None,
                Some(opt_s(input, "hiddenLabel").unwrap_or("Thinking...")),
                Some(opt_i(input, "outputPad").unwrap_or(1)),
                None,
                None,
                None,
            );
            c.update_content(from(input, "message"), Some(b(input, "streaming")));
            component(c)
        }
        "user_message" => component(UserMessageComponent::new(s(input, "text"), None, Some(opt_i(input, "outputPad").unwrap_or(1)), None, None, None)),
        "bash_execution" => {
            let c = BashExecutionComponent::new(
                s(input, "command"),
                test_tui(),
                Some(b(input, "excludeFromContext")),
                Some(opt_i(input, "outputPad").unwrap_or(1)),
            );
            if let Some(chunks) = input.get("chunks").and_then(Value::as_array) {
                for chunk in chunks {
                    c.append_output(chunk.as_str().unwrap_or(""));
                }
            }
            if let Some(complete) = input.get("complete") {
                let truncation: Option<TruncationResult> = opt_from(complete, "truncation");
                c.set_complete(
                    opt_i(complete, "exitCode"),
                    b(complete, "cancelled"),
                    truncation.as_ref(),
                    opt_s(complete, "fullOutputPath"),
                );
            }
            if b(input, "expanded") {
                c.set_expanded(true);
            }
            component(c)
        }
        "custom_message" => {
            let message: CustomMessage = from(input, "message");
            let c = CustomMessageComponent::new(&message, None, None, Some(opt_i(input, "outputPad").unwrap_or(1)));
            if b(input, "expanded") {
                c.set_expanded(true);
            }
            component(c)
        }
        "branch_summary" => {
            let message: BranchSummaryMessage = from(input, "message");
            let c = BranchSummaryMessageComponent::new(&message, None, Some(opt_i(input, "outputPad").unwrap_or(1)));
            if b(input, "expanded") {
                c.set_expanded(true);
            }
            component(c)
        }
        "compaction_summary" => {
            let message: CompactionSummaryMessage = from(input, "message");
            let c = CompactionSummaryMessageComponent::new(&message, None, Some(opt_i(input, "outputPad").unwrap_or(1)));
            if b(input, "expanded") {
                c.set_expanded(true);
            }
            component(c)
        }
        "skill_invocation" => {
            let block: ParsedSkillBlock = from(input, "block");
            let c = SkillInvocationMessageComponent::new(&block, None, Some(opt_i(input, "outputPad").unwrap_or(1)));
            if b(input, "expanded") {
                c.set_expanded(true);
            }
            component(c)
        }
        _ => return None,
    })
}
