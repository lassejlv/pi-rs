//! Agent session events and transcript rendering (TS `handleEvent`, `addMessageToChat`,
//! `renderSessionItems`, `renderSessionEntries` and the usage notices).

use super::*;
use crate::core::agent_session::AgentSessionEvent;
use crate::core::cache_stats::{
    CACHE_TTL_MS, CacheMiss, ModelPriceSource, ModelPriceSourceGetModelResult, ModelPriceSourceGetModelResultCost,
};
use crate::core::extensions::types::{CompactionReason, MarkdownTransformer};
use crate::core::messages::{CodingAgentMessage, create_compaction_summary_message, create_custom_message};
use crate::core::model_runtime::ModelRuntime;
use crate::core::session_manager::{CustomEntry, UsageEntry, session_entry_to_context_messages};
use crate::core::tools::js_util::js_to_fixed;
use crate::core::tools::truncate::TruncationResult;
use crate::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use crate::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use crate::modes::interactive::components::custom_entry::CustomEntryComponent;
use crate::modes::interactive::components::custom_message::CustomMessageComponent;
use crate::modes::interactive::components::footer::format_tokens;
use crate::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use crate::modes::interactive::components::status_indicator::{
    BranchSummaryStatusIndicator, CompactionStatusIndicator, CompactionStatusReason, RetryStatusIndicator,
};
use crate::modes::interactive::components::tool_execution::{
    ToolExecutionComponentToolDefinition, ToolExecutionComponentUpdateResultResult, ToolExecutionOptions,
};
use crate::modes::interactive::components::user_message::UserMessageComponent;
use pi_agent::types::AgentMessage;
use pi_ai::types::{AssistantContent, StopReason};
use serde_json::json;

/// [`ModelPriceSource`] view of the model runtime.
pub(crate) struct RuntimePrices(pub(crate) Arc<ModelRuntime>);

impl ModelPriceSource for RuntimePrices {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<ModelPriceSourceGetModelResult> {
        let model = self.0.get_model(provider, model_id)?;
        Some(ModelPriceSourceGetModelResult { cost: ModelPriceSourceGetModelResultCost { cache_read: model.cost.cache_read } })
    }
}

/// An item of the transcript as rendered from session entries.
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RenderItem {
    // Keep metadata attached to the converted message, even when entries have identical content.
    Message {
        message: AgentMessage,
        message_id: String,
        timestamp: String,
    },
    Custom(CustomEntry),
    Usage(UsageEntry),
    CompactionCost(CompactionCostNotice),
}

fn compaction_reason(reason: &CompactionReason) -> CompactionStatusReason {
    match reason {
        CompactionReason::Manual => CompactionStatusReason::Manual,
        CompactionReason::Threshold => CompactionStatusReason::Threshold,
        CompactionReason::Overflow => CompactionStatusReason::Overflow,
    }
}

fn aborted_message(retry_attempt: i64) -> String {
    if retry_attempt > 0 {
        format!("Aborted after {retry_attempt} retry attempt{}", if retry_attempt > 1 { "s" } else { "" })
    } else {
        "Operation aborted".to_string()
    }
}

fn error_result(text: &str) -> ToolExecutionComponentUpdateResultResult {
    serde_json::from_value(json!({ "content": [{ "type": "text", "text": text }], "isError": true }))
        .expect("tool result shape")
}

/// A tool result (or partial result) in the shape the tool row expects.
pub(crate) fn tool_result_value(
    content: &impl serde::Serialize,
    details: Option<&Value>,
    is_error: bool,
    duration_ms: Option<u64>,
) -> ToolExecutionComponentUpdateResultResult {
    let mut value = json!({ "content": content, "isError": is_error });
    if let Some(details) = details {
        value["details"] = details.clone();
    }
    if let Some(duration) = duration_ms {
        value["durationMs"] = json!(duration);
    }
    serde_json::from_value(value).unwrap_or_else(|_| ToolExecutionComponentUpdateResultResult {
        content: Vec::new(),
        details: details.cloned(),
        is_error,
        duration_ms: duration_ms.map(|d| d as f64),
    })
}

/// Extract text content from a user message.
pub(crate) fn user_message_text(message: &AgentMessage) -> String {
    let AgentMessage::User(user) = message else { return String::new() };
    match &user.content {
        pi_ai::types::UserContent::Text(text) => text.clone(),
        pi_ai::types::UserContent::Blocks(blocks) => {
            blocks.iter().filter_map(|b| b.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("")
        }
    }
}

impl Inner {
    pub(crate) fn subscribe_to_agent(&self) {
        let weak = self.weak.clone();
        let listener: crate::core::agent_session::AgentSessionEventListener = Arc::new(move |event| {
            if let Some(this) = weak.upgrade() {
                this.handle_event(event);
            }
        });
        let unsubscribe = self.session().subscribe(&listener);
        self.state.lock().unsubscribe = Some(unsubscribe);
    }

    /// Extension-registered renderers, falling back to the built-in ones.
    pub(crate) fn get_registered_tool_definition(&self, tool_name: &str) -> Option<ToolExecutionComponentToolDefinition> {
        let session = self.session();
        let base_session = session.clone();
        let name = tool_name.to_string();
        session
            .extension_runner()
            .resolve_tool_renderers(
                tool_name,
                Arc::new(move || {
                    crate::core::tools::renderers::with_built_in_renderers(
                        &name,
                        base_session.get_tool_definition(&name).map(|d| d.renderers()),
                    )
                }),
            )
            .map(ToolExecutionComponentToolDefinition::ToolRenderers)
    }

    pub(crate) fn get_markdown_transformers(&self) -> Vec<MarkdownTransformer> {
        let weak = self.weak.clone();
        let mermaid = crate::modes::interactive::components::mermaid::create_mermaid_markdown_transformer(
            &crate::modes::interactive::components::mermaid::MermaidTransformerOptions {
                get_mode: Arc::new(move || match weak.upgrade() {
                    Some(this) => this.settings_manager().get_mermaid_rendering_mode(),
                    None => crate::core::settings_schema::MermaidRenderingModeValue::Streaming,
                }),
                theme: None,
            },
        );
        let mut transformers = vec![mermaid];
        transformers.extend(self.session().extension_runner().get_markdown_transformers());
        transformers
    }

    fn new_tool_component(&self, tool_name: &str, tool_call_id: &str, args: Value) -> Arc<ToolExecutionComponent> {
        let settings = self.settings_manager();
        let (expanded, pad) = {
            let s = self.state.lock();
            (s.tool_output_expanded, s.output_pad)
        };
        let component = Arc::new(ToolExecutionComponent::new(
            tool_name,
            tool_call_id,
            args,
            Some(&ToolExecutionOptions {
                show_images: Some(settings.get_show_images()),
                image_width_cells: Some(settings.get_image_width_cells()),
                output_pad: Some(pad),
            }),
            self.get_registered_tool_definition(tool_name).as_ref(),
            self.ui(),
            &self.session_manager().get_cwd(),
        ));
        component.set_expanded(expanded);
        component
    }

    fn new_assistant_component(
        &self,
        message: Option<AssistantMessage>,
        message_id: Option<&str>,
        timestamp: Option<&str>,
    ) -> Arc<AssistantMessageComponent> {
        let (hide, label, pad) = {
            let s = self.state.lock();
            (s.hide_thinking_block, s.hidden_thinking_label.clone(), s.output_pad)
        };
        Arc::new(AssistantMessageComponent::new(
            message,
            Some(hide),
            Some(&self.get_markdown_theme_with_settings()),
            Some(&label),
            Some(pad),
            Some(&self.get_markdown_transformers()),
            message_id,
            timestamp,
        ))
    }

    /// TS `handleEvent`: initializes the mode first when an event arrives before `init()`.
    pub(crate) fn handle_event(&self, event: AgentSessionEvent) {
        if !self.state.lock().is_initialized {
            let this = self.this();
            self.start_now(async move {
                if let Err(error) = this.clone().init().await {
                    // The listener's promise rejects and nobody handles it.
                    this.uncaught_crash(&error);
                }
                this.handle_initialized_event(event);
            });
            return;
        }
        self.handle_initialized_event(event);
    }

    fn handle_initialized_event(&self, event: AgentSessionEvent) {
        // Agent events arrive on runtime threads while frames render on the TUI loop. TS applies
        // each event before the next frame; hold the UI lock so a frame never shows a component
        // rebuilt halfway (container cleared, children not yet re-added), which flickers.
        let _ui = pi_tui::ui_lock::lock_ui();
        self.footer.invalidate();
        self.program_status.handle_event(&event);
        let settings = self.settings_manager();

        match event {
            AgentSessionEvent::AgentStart => {
                let restore = {
                    let mut s = self.state.lock();
                    s.pending_tools.clear();
                    s.retry_escape_handler.take()
                };
                // Restore main escape handler if retry handler is still active
                if let Some(handler) = restore {
                    self.default_editor.set_on_escape(handler);
                }
            }
            AgentSessionEvent::TurnStart => {
                if settings.get_show_terminal_progress() {
                    self.ui().terminal().set_progress(true);
                }
                if self.state.lock().working_visible {
                    if self.active_indicator_kind() != Some(StatusIndicatorKind::Working) {
                        self.show_working_status_indicator();
                    }
                } else {
                    self.clear_status_indicator(None);
                }
                self.request_render();
            }
            AgentSessionEvent::QueueUpdate { .. } => {
                self.update_pending_messages_display();
                self.request_render();
            }
            AgentSessionEvent::EntryAppended { entry } => self.handle_entry_appended(entry),
            AgentSessionEvent::SessionInfoChanged { .. } => {
                self.update_terminal_title();
                self.footer.invalidate();
                self.request_render();
            }
            AgentSessionEvent::ThinkingLevelChanged { .. } => {
                self.footer.invalidate();
                self.update_editor_border_color();
            }
            AgentSessionEvent::MessageStart { message } => match &message {
                AgentMessage::Custom(_) => {
                    self.add_message_to_chat(&message, false);
                    self.request_render();
                }
                AgentMessage::User(_) => {
                    self.add_message_to_chat(&message, false);
                    self.update_pending_messages_display();
                    self.request_render();
                }
                AgentMessage::Assistant(assistant) => {
                    let component = self.new_assistant_component(None, None, None);
                    {
                        let mut s = self.state.lock();
                        s.streaming_component = Some(component.clone());
                        s.streaming_message = Some(assistant.clone());
                    }
                    self.chat_container.add_child(component.clone());
                    component.update_content(assistant.clone(), Some(true));
                    self.request_render();
                }
                _ => {}
            },
            AgentSessionEvent::MessageUpdate { message, .. } => {
                let AgentMessage::Assistant(assistant) = &message else { return };
                let Some(streaming) = self.state.lock().streaming_component.clone() else { return };
                self.state.lock().streaming_message = Some(assistant.clone());
                streaming.update_content(assistant.clone(), Some(true));
                for content in &assistant.content {
                    let AssistantContent::ToolCall(call) = content else { continue };
                    let args = Value::Object(call.arguments.clone().into_iter().collect());
                    let existing = self.state.lock().pending_tools.get(&call.id).cloned();
                    match existing {
                        Some(component) => component.update_args(args),
                        None => {
                            let component = self.new_tool_component(&call.name, &call.id, args);
                            self.chat_container.add_child(component.clone());
                            self.state.lock().pending_tools.insert(call.id.clone(), component);
                        }
                    }
                }
                self.request_render();
            }
            AgentSessionEvent::MessageEnd { message } => {
                if matches!(message, AgentMessage::User(_)) {
                    return;
                }
                // Bind first: a guard in an `if let` scrutinee would stay locked for the whole block.
                let streaming_component = self.state.lock().streaming_component.clone();
                if let AgentMessage::Assistant(assistant) = &message
                    && let Some(streaming) = streaming_component
                {
                    let mut assistant = assistant.clone();
                    let mut error_message: Option<String> = None;
                    if assistant.stop_reason == StopReason::Aborted {
                        let text = aborted_message(self.session().retry_attempt());
                        assistant.error_message = Some(text.clone());
                        error_message = Some(text);
                        // TS writes this into the shared message object, so agent state, the
                        // persisted entry and a later chat rebuild show it too.
                        self.session().replace_message_end_message(AgentMessage::Assistant(assistant.clone()));
                    }
                    streaming.update_content(assistant.clone(), Some(false));
                    if matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error) {
                        let error_message = error_message.unwrap_or_else(|| {
                            assistant.error_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "Error".to_string())
                        });
                        let tools: Vec<_> = std::mem::take(&mut self.state.lock().pending_tools).into_values().collect();
                        for component in tools {
                            component.update_result(&error_result(&error_message), None);
                        }
                        self.maybe_suggest_bug_report(&assistant);
                    } else {
                        // Args are now complete - trigger diff computation for edit tools
                        let tools: Vec<_> = self.state.lock().pending_tools.values().cloned().collect();
                        for component in tools {
                            component.set_args_complete();
                        }
                        self.maybe_show_thinking_drop_notice(&assistant);
                        self.maybe_show_cache_miss_notice(&assistant);
                    }
                    {
                        let mut s = self.state.lock();
                        s.streaming_component = None;
                        s.streaming_message = None;
                    }
                    self.footer.invalidate();
                }
                self.request_render();
            }
            AgentSessionEvent::BashExecutionUpdate { .. } => {
                // The bash execution callback handles TUI output rendering.
            }
            AgentSessionEvent::ToolExecutionStart { tool_call_id, tool_name, args, parent_tool_call_id } => {
                // Nested calls (from codemode scripts) are shown inside their parent's row.
                if parent_tool_call_id.is_some() {
                    return;
                }
                let existing = self.state.lock().pending_tools.get(&tool_call_id).cloned();
                let component = match existing {
                    Some(component) => component,
                    None => {
                        let component = self.new_tool_component(&tool_name, &tool_call_id, args);
                        self.chat_container.add_child(component.clone());
                        self.state.lock().pending_tools.insert(tool_call_id.clone(), component.clone());
                        component
                    }
                };
                component.mark_execution_started();
                self.request_render();
            }
            AgentSessionEvent::ToolExecutionUpdate { tool_call_id, partial_result, .. } => {
                let component = self.state.lock().pending_tools.get(&tool_call_id).cloned();
                if let Some(component) = component {
                    let result = tool_result_value(&partial_result.content, partial_result.details.as_ref(), false, None);
                    component.update_result(&result, Some(true));
                    self.request_render();
                }
            }
            AgentSessionEvent::ToolExecutionEnd { tool_call_id, result, is_error, duration_ms, .. } => {
                if is_error {
                    self.maybe_show_install_change_warning();
                }
                let component = self.state.lock().pending_tools.shift_remove(&tool_call_id);
                if let Some(component) = component {
                    let value = tool_result_value(&result.content, result.details.as_ref(), is_error, duration_ms);
                    component.update_result(&value, None);
                    self.request_render();
                }
            }
            AgentSessionEvent::AgentEnd { .. } => {
                if settings.get_show_terminal_progress() {
                    self.ui().terminal().set_progress(false);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Working));
                let streaming = {
                    let mut s = self.state.lock();
                    s.streaming_message = None;
                    s.pending_tools.clear();
                    s.streaming_component.take()
                };
                if let Some(streaming) = streaming {
                    let component: ComponentRef = streaming;
                    self.chat_container.remove_child(&component);
                }
                self.request_render();
            }
            AgentSessionEvent::AgentSettled { .. } => {
                let this = self.this();
                self.spawn(async move { this.check_shutdown_requested().await });
            }
            AgentSessionEvent::CompactionStart { reason } => {
                if settings.get_show_terminal_progress() {
                    self.ui().terminal().set_progress(true);
                }
                // Keep editor active; submissions are queued during compaction.
                let previous = self.default_editor.on_escape();
                self.state.lock().auto_compaction_escape_handler = Some(previous);
                let weak = self.weak.clone();
                self.default_editor.set_on_escape(Some(Arc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.session().abort_compaction();
                    }
                })));
                let indicator =
                    CompactionStatusIndicator::new(self.ui(), &compaction_reason(&reason), self.embedded_indicator_color());
                self.show_status_indicator(indicator.indicator());
                self.request_render();
            }
            AgentSessionEvent::CompactionEnd { reason, result, aborted, will_retry, error_message } => {
                if settings.get_show_terminal_progress() {
                    self.ui().terminal().set_progress(false);
                }
                let restore = self.state.lock().auto_compaction_escape_handler.take();
                if let Some(handler) = restore {
                    self.default_editor.set_on_escape(handler);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Compaction));
                if aborted {
                    if reason == CompactionReason::Manual {
                        self.show_error("Compaction cancelled");
                    } else {
                        self.show_status("Auto-compaction cancelled");
                    }
                } else if let Some(result) = result {
                    let entries = self.session_manager().build_context_entries();
                    if !matches!(entries.first(), Some(SessionEntry::Compaction(_))) {
                        self.show_error("Completed compaction is missing from the session context");
                    } else {
                        self.chat_container.clear();
                        // The latest compaction is prepended for model context; append it below at its chronological position.
                        self.render_session_entries(&entries[1..], false, false);
                        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                        let summary = create_compaction_summary_message(&result.summary, result.tokens_before, &now);
                        self.add_message_to_chat(&summary.into(), false);
                        if let Some(usage) = &result.usage {
                            self.add_compaction_cost_notice(&CompactionCostNotice {
                                r#type: "compaction_cost".to_string(),
                                kind: CompactionCostNoticeKind::Compaction,
                                usage: usage.clone(),
                            });
                        }
                        self.footer.invalidate();
                    }
                } else if let Some(error_message) = error_message {
                    if reason == CompactionReason::Manual {
                        self.show_error(&error_message);
                    } else {
                        self.add_spacer();
                        self.add_text(
                            &self.chat_container,
                            Arc::new(move || theme().fg(&ThemeColor::Error, &error_message)),
                            1,
                            0,
                        );
                    }
                }
                let this = self.this();
                self.spawn(async move { this.flush_compaction_queue(will_retry).await });
                self.request_render();
            }
            AgentSessionEvent::AutoRetryStart { attempt, max_attempts, delay_ms, .. } => {
                let previous = self.default_editor.on_escape();
                self.state.lock().retry_escape_handler = Some(previous);
                let weak = self.weak.clone();
                self.default_editor.set_on_escape(Some(Arc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.session().abort_retry();
                    }
                })));
                let indicator = RetryStatusIndicator::new(self.ui(), attempt as i64, max_attempts as i64, delay_ms as i64);
                self.show_status_indicator(indicator.indicator());
                self.request_render();
            }
            AgentSessionEvent::AutoRetryEnd { success, attempt, final_error } => {
                let restore = self.state.lock().retry_escape_handler.take();
                if let Some(handler) = restore {
                    self.default_editor.set_on_escape(handler);
                }
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                if !success {
                    self.show_error(&format!(
                        "Retry failed after {attempt} attempts: {}",
                        final_error.filter(|e| !e.is_empty()).unwrap_or_else(|| "Unknown error".to_string())
                    ));
                }
                self.request_render();
            }
            AgentSessionEvent::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message } => {
                self.show_error(&error_message);
                let indicator = RetryStatusIndicator::new(self.ui(), attempt as i64, max_attempts as i64, delay_ms as i64);
                self.show_status_indicator(indicator.indicator());
                self.request_render();
            }
            AgentSessionEvent::SummarizationRetryAttemptStart(source) => {
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                match source {
                    crate::core::agent_session::SummarizationRetrySource::BranchSummary => {
                        self.show_status_indicator(BranchSummaryStatusIndicator::new(self.ui()).indicator());
                    }
                    crate::core::agent_session::SummarizationRetrySource::Compaction { reason } => {
                        let indicator = CompactionStatusIndicator::new(
                            self.ui(),
                            &compaction_reason(&reason),
                            self.embedded_indicator_color(),
                        );
                        self.show_status_indicator(indicator.indicator());
                    }
                }
                self.request_render();
            }
            AgentSessionEvent::SummarizationRetryFinished => {
                self.clear_status_indicator(Some(StatusIndicatorKind::Retry));
                self.request_render();
            }
            AgentSessionEvent::TurnEnd { .. } => {}
        }
    }

    fn handle_entry_appended(&self, entry: SessionEntry) {
        let id = entry_id(&entry);
        if self.state.lock().entries_rendered_by_boundary_compaction.remove(&id) {
            return;
        }
        match &entry {
            SessionEntry::Custom(custom) => {
                self.add_custom_entry_to_chat(custom);
                self.request_render();
            }
            SessionEntry::Usage(usage) if usage.kind == "cache_warm" => {
                self.add_cache_warming_usage(usage);
                self.request_render();
            }
            SessionEntry::CustomMessage(message) if message.display => {
                let custom = create_custom_message(
                    &message.custom_type,
                    message.content.clone(),
                    message.display,
                    message.details.clone(),
                    &message.timestamp,
                );
                self.add_message_to_chat(&custom.into(), false);
                self.request_render();
            }
            SessionEntry::Compaction(compaction) => {
                let sm = self.session_manager();
                let entries = sm.build_context_entries();
                if entries.first().map(entry_id).as_deref() != Some(compaction.id.as_str()) {
                    return;
                }
                self.chat_container.clear();
                let branch = sm.get_branch(None);
                let compaction_index = branch.iter().position(|e| entry_id(e) == compaction.id);
                let after: HashSet<String> = match compaction_index {
                    Some(index) => branch[index + 1..].iter().map(entry_id).collect(),
                    None => HashSet::new(),
                };
                let retained = &entries[1..];
                let before_entries: Vec<SessionEntry> =
                    retained.iter().filter(|e| !after.contains(&entry_id(e))).cloned().collect();
                self.render_session_entries(&before_entries, false, false);
                let summary =
                    create_compaction_summary_message(&compaction.summary, compaction.tokens_before, &compaction.timestamp);
                self.add_message_to_chat(&summary.into(), false);
                if let Some(usage) = &compaction.usage {
                    self.add_compaction_cost_notice(&CompactionCostNotice {
                        r#type: "compaction_cost".to_string(),
                        kind: CompactionCostNoticeKind::Compaction,
                        usage: usage.clone(),
                    });
                }
                let after_entries: Vec<SessionEntry> =
                    retained.iter().filter(|e| after.contains(&entry_id(e))).cloned().collect();
                self.render_session_entries(&after_entries, false, false);
                self.state.lock().entries_rendered_by_boundary_compaction.extend(after);
                self.footer.invalidate();
                self.request_render();
            }
            _ => {}
        }
    }

    pub(crate) fn add_custom_entry_to_chat(&self, entry: &CustomEntry) {
        let Some(renderer) = self.session().extension_runner().get_entry_renderer(&entry.custom_type) else {
            return;
        };
        let (expanded, pad, streaming) = {
            let s = self.state.lock();
            (s.tool_output_expanded, s.output_pad, s.streaming_component.clone())
        };
        let component = Arc::new(CustomEntryComponent::new(entry, &renderer, Some(pad)));
        component.set_expanded(expanded);
        if !component.has_content() {
            return;
        }
        if let Some(streaming) = streaming {
            let streaming: ComponentRef = streaming;
            let children = self.chat_container.children();
            if let Some(index) = children.iter().position(|c| pi_tui::tui::same_component(c, &streaming)) {
                self.chat_container.insert_child(index, component);
                return;
            }
        }
        self.chat_container.add_child(component);
    }

    pub(crate) fn add_message_to_chat(&self, message: &AgentMessage, populate_history: bool) {
        self.add_message_to_chat_with_meta(message, populate_history, None, None);
    }

    fn add_message_to_chat_with_meta(
        &self,
        message: &AgentMessage,
        populate_history: bool,
        message_id: Option<&str>,
        timestamp: Option<&str>,
    ) {
        let (expanded, pad) = {
            let s = self.state.lock();
            (s.tool_output_expanded, s.output_pad)
        };
        match message {
            AgentMessage::Custom(_) => match CodingAgentMessage::from_agent_message(message) {
                Some(CodingAgentMessage::BashExecution(bash)) => {
                    let component =
                        Arc::new(BashExecutionComponent::new(&bash.command, self.ui(), bash.exclude_from_context, Some(pad)));
                    if !bash.output.is_empty() {
                        component.append_output(&bash.output);
                    }
                    let truncation = bash.truncated.then(|| truncated_marker(""));
                    component.set_complete(bash.exit_code, bash.cancelled, truncation.as_ref(), bash.full_output_path.as_deref());
                    self.chat_container.add_child(component);
                }
                Some(CodingAgentMessage::Custom(custom)) => {
                    if custom.display {
                        let renderer = self.session().extension_runner().get_message_renderer(&custom.custom_type);
                        let component = Arc::new(CustomMessageComponent::new(
                            &custom,
                            renderer.as_ref(),
                            Some(&self.get_markdown_theme_with_settings()),
                            Some(pad),
                        ));
                        component.set_expanded(expanded);
                        self.chat_container.add_child(component);
                    }
                }
                Some(CodingAgentMessage::CompactionSummary(summary)) => {
                    self.add_spacer();
                    let component = Arc::new(CompactionSummaryMessageComponent::new(
                        &summary,
                        Some(&self.get_markdown_theme_with_settings()),
                        Some(pad),
                    ));
                    component.set_expanded(expanded);
                    self.chat_container.add_child(component);
                }
                Some(CodingAgentMessage::BranchSummary(summary)) => {
                    self.add_spacer();
                    let component = Arc::new(BranchSummaryMessageComponent::new(
                        &summary,
                        Some(&self.get_markdown_theme_with_settings()),
                        Some(pad),
                    ));
                    component.set_expanded(expanded);
                    self.chat_container.add_child(component);
                }
                None => {}
            },
            AgentMessage::System(_) => {}
            AgentMessage::User(_) => {
                let text = user_message_text(message);
                if text.is_empty() {
                    return;
                }
                if !self.chat_container.children().is_empty() {
                    self.add_spacer();
                }
                let transformers = self.get_markdown_transformers();
                if let Some(skill_block) = crate::core::agent_session::parse_skill_block(&text) {
                    // Render skill block (collapsible)
                    let component = Arc::new(SkillInvocationMessageComponent::new(
                        &skill_block,
                        Some(&self.get_markdown_theme_with_settings()),
                        Some(pad),
                    ));
                    component.set_expanded(expanded);
                    self.chat_container.add_child(component);
                    // Render user message separately if present
                    if let Some(user_message) = &skill_block.user_message {
                        self.add_spacer();
                        self.chat_container.add_child(Arc::new(UserMessageComponent::new(
                            user_message,
                            Some(&self.get_markdown_theme_with_settings()),
                            Some(pad),
                            Some(&transformers),
                            message_id,
                            timestamp,
                        )));
                    }
                } else {
                    self.chat_container.add_child(Arc::new(UserMessageComponent::new(
                        &text,
                        Some(&self.get_markdown_theme_with_settings()),
                        Some(pad),
                        Some(&transformers),
                        message_id,
                        timestamp,
                    )));
                }
                if populate_history {
                    self.editor().add_to_history(&text);
                }
            }
            AgentMessage::Assistant(assistant) => {
                let component = self.new_assistant_component(Some(assistant.clone()), message_id, timestamp);
                self.chat_container.add_child(component);
            }
            AgentMessage::ToolResult(_) => {
                // Tool results are rendered inline with tool calls, handled separately
            }
        }
    }

    pub(crate) fn render_session_items(&self, items: Vec<RenderItem>, update_footer: bool, populate_history: bool) {
        self.state.lock().pending_tools.clear();
        let mut rendered_pending_tools: IndexMap<String, Arc<ToolExecutionComponent>> = IndexMap::new();
        let settings = self.settings_manager();
        let session = self.session();
        let sm = self.session_manager();
        // Cache misses are not persisted, unlike successful cache-warming usage.
        let cache_misses = if settings.get_show_cache_miss_notices() {
            crate::core::cache_stats::collect_cache_misses(&sm.get_entries(), Arc::new(RuntimePrices(session.model_runtime())))
        } else {
            IndexMap::new()
        };
        // Map assistant messages back to their entries (TS keys misses by message reference).
        let message_entry_ids: Vec<(AssistantMessage, String)> = if cache_misses.is_empty() {
            Vec::new()
        } else {
            sm.get_entries()
                .into_iter()
                .filter_map(|e| match e {
                    SessionEntry::Message(m) => match m.message {
                        AgentMessage::Assistant(a) => Some((a, m.id)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        };

        if update_footer {
            self.footer.invalidate();
            self.update_editor_border_color();
        }

        for item in items {
            match item {
                RenderItem::Custom(entry) => self.add_custom_entry_to_chat(&entry),
                RenderItem::Usage(entry) => self.add_cache_warming_usage(&entry),
                RenderItem::CompactionCost(notice) => self.add_compaction_cost_notice(&notice),
                RenderItem::Message { message, message_id, timestamp } => match &message {
                    AgentMessage::Assistant(assistant) => {
                        self.add_message_to_chat_with_meta(&message, false, Some(&message_id), Some(&timestamp));
                        for content in &assistant.content {
                            let AssistantContent::ToolCall(call) = content else { continue };
                            let args = Value::Object(call.arguments.clone().into_iter().collect());
                            let component = self.new_tool_component(&call.name, &call.id, args);
                            self.chat_container.add_child(component.clone());
                            if matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error) {
                                let error_message = if assistant.stop_reason == StopReason::Aborted {
                                    aborted_message(session.retry_attempt())
                                } else {
                                    assistant.error_message.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "Error".to_string())
                                };
                                component.update_result(&error_result(&error_message), None);
                            } else {
                                rendered_pending_tools.insert(call.id.clone(), component);
                            }
                        }
                        if !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error) {
                            let entry_id = message_entry_ids.iter().find(|(m, _)| m == assistant).map(|(_, id)| id.clone());
                            if let Some(miss) = entry_id.and_then(|id| cache_misses.get(&id)) {
                                self.add_cache_miss_notice(miss);
                            }
                        }
                    }
                    AgentMessage::ToolResult(result) => {
                        if let Some(component) = rendered_pending_tools.shift_remove(&result.tool_call_id) {
                            let value = tool_result_value(&result.content, result.details.as_ref(), result.is_error, None);
                            component.update_result(&value, None);
                        }
                    }
                    _ => self.add_message_to_chat_with_meta(&message, populate_history, Some(&message_id), Some(&timestamp)),
                },
            }
        }

        self.state.lock().pending_tools.extend(rendered_pending_tools);
        self.request_render();
    }

    /// Render session entries to chat. Used for initial load and rebuild after compaction.
    pub(crate) fn render_session_entries(&self, entries: &[SessionEntry], update_footer: bool, populate_history: bool) {
        // Selection coordinates point into the transcript being replaced.
        let ui = self.ui();
        if ui.is_viewport_tui() {
            ui.reset_text_selection();
        }
        let mut items = Vec::new();
        for entry in entries {
            match entry {
                SessionEntry::Custom(custom) => items.push(RenderItem::Custom(custom.clone())),
                SessionEntry::Usage(usage) if usage.kind == "cache_warm" => items.push(RenderItem::Usage(usage.clone())),
                _ => {
                    let messages = session_entry_to_context_messages(entry);
                    let cost = match entry {
                        SessionEntry::Compaction(c) => c.usage.clone().map(|u| (CompactionCostNoticeKind::Compaction, u)),
                        SessionEntry::BranchSummary(b) => b.usage.clone().map(|u| (CompactionCostNoticeKind::BranchSummary, u)),
                        _ => None,
                    };
                    let has_messages = !messages.is_empty();
                    items.extend(messages.into_iter().map(|message| RenderItem::Message {
                        message,
                        message_id: entry.id().to_string(),
                        timestamp: entry.timestamp().to_string(),
                    }));
                    if let Some((kind, usage)) = cost
                        && has_messages
                    {
                        items.push(RenderItem::CompactionCost(CompactionCostNotice {
                            r#type: "compaction_cost".to_string(),
                            kind,
                            usage,
                        }));
                    }
                }
            }
        }
        self.render_session_items(items, update_footer, populate_history);
    }

    pub(crate) fn add_cache_warming_usage(&self, entry: &UsageEntry) {
        if !self.settings_manager().get_show_cache_miss_notices() {
            return;
        }
        self.add_spacer();
        let usage = crate::core::cache_warmer::format_cache_warming_usage(entry);
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Dim, &usage)), 1, 0);
    }

    /// Render billing usage for a compaction or branch summary.
    pub(crate) fn add_compaction_cost_notice(&self, notice: &CompactionCostNotice) {
        if !self.settings_manager().get_show_cache_miss_notices() {
            return;
        }
        let usage = &notice.usage;
        let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        let cost = if usage.cost.total >= 0.01 { format!(" (~${})", js_to_fixed(usage.cost.total, 2)) } else { String::new() };
        let label = match notice.kind {
            CompactionCostNoticeKind::Compaction => "Compaction",
            CompactionCostNoticeKind::BranchSummary => "Branch summary",
        };
        let text = format!("{label}: {} tokens billed{cost}", format_tokens(tokens as i64));
        self.add_spacer();
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Warning, &text)), 1, 0);
    }

    fn count_dropped_thinking_blocks(message: &AssistantMessage) -> usize {
        let mut count = 0;
        for diagnostic in message.diagnostics.iter().flatten() {
            if diagnostic.diagnostic_type != "anthropic_input_transformations" {
                continue;
            }
            let Some(Value::Array(transformations)) = diagnostic.details.as_ref().and_then(|d| d.get("transformations")) else {
                continue;
            };
            count += transformations
                .iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("thinking_dropped"))
                .count();
        }
        count
    }

    fn maybe_show_thinking_drop_notice(&self, message: &AssistantMessage) {
        if !self.settings_manager().get_show_cache_miss_notices() {
            return;
        }
        let dropped = Self::count_dropped_thinking_blocks(message);
        if dropped == 0 {
            return;
        }
        // message_end reaches the UI before the current message is persisted,
        // so the branch's last assistant message is the previous response.
        let previous = self
            .session_manager()
            .get_branch(None)
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEntry::Message(m) => match &m.message {
                    AgentMessage::Assistant(a) => Some(Self::count_dropped_thinking_blocks(a)),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(0);
        if dropped <= previous {
            return;
        }
        let noun = if dropped == 1 { "thinking block" } else { "thinking blocks" };
        let text = format!("Anthropic dropped {dropped} {noun} (details in session)");
        self.add_spacer();
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Warning, &text)), 1, 0);
    }

    /// Show a transcript notice when a completed assistant message paid for a significant cache miss.
    fn maybe_show_cache_miss_notice(&self, message: &AssistantMessage) {
        if !self.settings_manager().get_show_cache_miss_notices() {
            return;
        }
        // Entries don't contain `message` yet: message_end fires before persistence.
        let miss = crate::core::cache_stats::detect_cache_miss(
            &self.session_manager().get_entries(),
            message,
            Arc::new(RuntimePrices(self.session().model_runtime())),
        );
        if let Some(miss) = miss {
            self.add_cache_miss_notice(&miss);
        }
    }

    fn add_cache_miss_notice(&self, miss: &CacheMiss) {
        if miss.missed_tokens < 20_000 && miss.missed_cost < 0.1 {
            return;
        }
        let cost = if miss.missed_cost >= 0.01 { format!(" (~${})", js_to_fixed(miss.missed_cost, 2)) } else { String::new() };
        let re_billed = format!("{} tokens re-billed{cost}", format_tokens(miss.missed_tokens as i64));
        let label = if miss.model_changed {
            "Cache miss after model switch".to_string()
        } else if miss.idle_ms as i64 >= CACHE_TTL_MS {
            format!("Cache miss after {}m idle", (miss.idle_ms / 60_000.0).round() as i64)
        } else {
            "Cache miss".to_string()
        };
        let text = format!("{label}: {re_billed}");
        self.add_spacer();
        self.add_text(&self.chat_container, Arc::new(move || theme().fg(&ThemeColor::Warning, &text)), 1, 0);
    }
}

/// The id of a session entry.
pub(crate) fn entry_id(entry: &SessionEntry) -> String {
    entry.id().to_string()
}

/// `{ truncated: true, content }` as a truncation result (TS casts a partial object).
pub(crate) fn truncated_marker(content: &str) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: true,
        truncated_by: None,
        total_lines: 0,
        total_bytes: 0,
        output_lines: 0,
        output_bytes: 0,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines: 0,
        max_bytes: 0,
    }
}
