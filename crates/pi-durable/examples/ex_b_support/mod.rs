//! Helpers shared by the offline ports of the upstream coding-agent examples.
#![allow(dead_code)]
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxMessageOptions, FauxProviderHandle, FauxResponseStep, RegisterFauxProviderOptions,
        faux_assistant_message, faux_provider, faux_text, faux_tool_call,
    },
};
use pi_durable::{
    env::{ExecutionEnv, NativeExecutionEnv},
    harness::declaration::ToolDeclaration,
    harness::{EnvTarget, EnvironmentFactory},
    *,
};
use std::{path::PathBuf, sync::Arc};

pub type Reg = Registry<NativeTool, NativeSection>;
pub type Ext = Extension<NativeTool, NativeSection>;

pub fn json(text: &str) -> JsonValue {
    pi_chord::json::parse(text).expect("valid JSON")
}
pub fn object(text: &str) -> JsonObject {
    JsonObject::try_from_value(json(text)).expect("JSON object")
}
/// Convert durable JSON to `serde_json` for easy inspection.
pub fn sj(value: &JsonValue) -> serde_json::Value {
    serde_json::from_str(&value.to_string()).expect("serde JSON")
}
pub fn from_sj(value: serde_json::Value) -> JsonValue {
    json(&value.to_string())
}
pub fn registry() -> Reg {
    create_registry().expect("registry")
}
pub fn install(registry: &Reg, extension: Ext) {
    registry.install(Arc::new(extension)).expect("install");
}
pub fn ctx() -> Context {
    Context::default()
}

/// The faux provider, installed into a fresh model collection.
pub fn faux(options: RegisterFauxProviderOptions) -> (Models, FauxProviderHandle) {
    let models = Models::default();
    let faux = faux_provider(options);
    models.set_provider(faux.provider.clone());
    (models, faux)
}
pub fn fast() -> RegisterFauxProviderOptions {
    RegisterFauxProviderOptions {
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    }
}
pub fn tool_use() -> FauxMessageOptions {
    FauxMessageOptions {
        stop_reason: Some(pi_ai::StopReason::ToolUse),
        ..Default::default()
    }
}
/// A scripted assistant turn that calls one tool.
pub fn call(name: &str, args: serde_json::Value, id: Option<&str>) -> FauxResponseStep {
    let args = args.as_object().cloned().unwrap_or_default();
    faux_assistant_message(vec![faux_tool_call(name, args, id)], tool_use()).into()
}
pub fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(vec![faux_text(text)], Default::default()).into()
}

/// Agent configuration for a conversation: `{"model": faux/<id>, ...extra}`.
pub fn agent(model_id: &str, extra: serde_json::Value) -> JsonObject {
    let mut value = serde_json::json!({"model": {"provider": "faux", "modelId": model_id}});
    if let Some(extra) = extra.as_object() {
        for (key, item) in extra {
            value[key] = item.clone();
        }
    }
    JsonObject::try_from_value(from_sj(value)).expect("agent object")
}
pub fn root_options(model_id: &str, extra: serde_json::Value) -> ConversationOptions {
    ConversationOptions {
        agent: Some(agent(model_id, extra)),
        ..Default::default()
    }
}

/// An environment factory that gives every conversation the same directory, or the
/// conversation's own `cwd` when it has one.
pub fn cwd_env(default: PathBuf) -> EnvironmentFactory {
    Arc::new(move |target: EnvTarget, _| {
        let cwd = target
            .cwd
            .as_ref()
            .map(|cwd| PathBuf::from(cwd.to_string()))
            .unwrap_or_else(|| default.clone());
        Box::pin(async move { Ok(Some(native_env(&cwd)?)) })
    })
}
pub fn native_env(cwd: &std::path::Path) -> Result<Arc<dyn ExecutionEnv>> {
    Ok(Arc::new(
        NativeExecutionEnv::new(cwd).map_err(|error| Error::message(error.to_string()))?,
    ))
}

pub fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}
/// First model message of an entry as `serde_json`.
pub fn message(entry: &EntryRecord) -> Option<serde_json::Value> {
    entry.model.as_ref()?.first().map(sj)
}
pub fn message_text(entry: &EntryRecord) -> String {
    message(entry).map_or_else(String::new, |message| content_text(&message["content"]))
}
/// Entries oldest first.
pub async fn transcript(conversation: &Conversation, limit: usize) -> Result<Vec<EntryRecord>> {
    let mut page = conversation
        .entries(Default::default(), limit, None, &ctx())
        .await?
        .items;
    if page.len() > 1 && page[0].id.get() > page[page.len() - 1].id.get() {
        page.reverse();
    }
    Ok(page)
}
pub fn is_kind(entry: &EntryRecord, kind: &str) -> bool {
    entry.kind.equals_str(kind)
}
/// Text of the last tool result in a conversation.
pub async fn last_tool_result(conversation: &Conversation) -> Result<String> {
    let entries = transcript(conversation, 100).await?;
    let entry = entries
        .iter()
        .rev()
        .find(|entry| is_kind(entry, "pi.tool-result"))
        .ok_or_else(|| Error::message("No tool result"))?;
    Ok(message_text(entry).trim().to_string())
}
/// The assistant message a settled input submission answered with.
pub async fn answer_of(
    conversation: &Conversation,
    settled: &SettledSubmissionRecord,
) -> Result<Option<serde_json::Value>> {
    let SubmissionState::InputDone { answer, .. } = &settled.as_record().state else {
        return Ok(None);
    };
    let answer = *answer;
    let entry = conversation
        .commit(move |tx| async move { tx.entry(answer).await }, &ctx())
        .await?;
    Ok(entry.as_ref().and_then(message))
}
pub async fn answer_text(
    conversation: &Conversation,
    settled: &SettledSubmissionRecord,
) -> Result<String> {
    Ok(answer_of(conversation, settled)
        .await?
        .map_or_else(String::new, |message| content_text(&message["content"])))
}
pub fn status_of(settled: &SettledSubmissionRecord) -> SubmissionStatus {
    settled.as_record().state.status()
}
pub fn check(condition: bool, what: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::message(format!("Assertion failed: {what}")))
    }
}
pub async fn pause(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// A "coding" extension: read and bash tools plus a concise preamble section.
pub fn coding_extension(name: &str) -> Ext {
    use pi_durable::tools::{create_bash_tool, create_read_tool};
    let mut extension = Ext::new(name);
    extension.tools = vec![
        Arc::new(create_read_tool(Default::default())),
        Arc::new(create_bash_tool(Default::default())),
    ];
    extension.sections.push(Arc::new(section(
        "preamble",
        Arc::new(|_, _| Box::pin(async { Ok(Some("You are a concise coding assistant.".into())) })),
        SectionOptions { tag: Some(false) },
    )));
    extension
}
/// A temp directory with a few files, so `ls` has stable output.
pub fn sample_project() -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    for name in ["README.md", "package.json", "src.txt"] {
        std::fs::write(directory.path().join(name), name)
            .map_err(|e| Error::message(e.to_string()))?;
    }
    Ok(directory)
}
/// A section with static text.
pub fn static_section(key: &str, text: &'static str) -> NativeSection {
    section(
        key,
        Arc::new(move |_, _| Box::pin(async move { Ok(Some(text.into())) })),
        SectionOptions::default(),
    )
}

/// An input submission with explicit busy handling.
pub fn input_when(content: &str, when_busy: WhenBusy) -> SubmissionDraft {
    SubmissionDraft::Input {
        request_id: None,
        content: content.into(),
        when_busy: Some(when_busy),
        entry: None,
    }
}
/// An input submission with a request ID, so a repeat returns the first one.
pub fn input_request(
    content: &str,
    request_id: &str,
    when_busy: Option<WhenBusy>,
) -> SubmissionDraft {
    SubmissionDraft::Input {
        request_id: Some(request_id.into()),
        content: content.into(),
        when_busy,
        entry: None,
    }
}
/// The settle reason of an unanswered submission.
pub fn unanswered_reason(settled: &SettledSubmissionRecord) -> Option<String> {
    match &settled.as_record().state {
        SubmissionState::InputUnanswered { reason, .. }
        | SubmissionState::WriteUnanswered { reason, .. } => Some(reason.to_string()),
        _ => None,
    }
}
/// A faux response that waits for `gate` before answering with `text`.
pub fn held_answer(gate: Arc<tokio::sync::Notify>, text: &'static str) -> FauxResponseStep {
    FauxResponseStep::Factory(Arc::new(move |_, _, _, _| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(pi_ai::providers::faux::faux_text_message(text))
        })
    }))
}

/// A tool from a name, description, JSON-schema parameters and an async body.
pub fn tool<F>(name: &str, description: &str, parameters: &str, execute: F) -> NativeTool
where
    F: Fn(
            JsonObject,
            ToolExecutionApi,
            Context,
        ) -> pi_durable::storage::StorageFuture<ToolExecutionResult>
        + Send
        + Sync
        + 'static,
{
    NativeTool::new(
        ToolDeclaration::new(name, description, json(parameters)),
        Arc::new(execute),
    )
}
pub fn text_block(text: &str) -> Arc<JsonObject> {
    Arc::new(object(
        &serde_json::json!({"type": "text", "text": text}).to_string(),
    ))
}
pub fn text_result(text: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Arc::new(vec![text_block(text)])),
        ..Default::default()
    }
}

/// The terminal outcome of a task record, as the status word.
pub fn outcome_status<R>(record: &TaskRecord<JsonValue, JsonValue, R>) -> &'static str {
    match &record.state {
        TaskState::Terminal { outcome } => outcome_word(outcome),
        _ => "not-terminal",
    }
}
pub fn outcome_word<R>(outcome: &TaskOutcome<R>) -> &'static str {
    match outcome {
        TaskOutcome::Completed { .. } => "completed",
        TaskOutcome::Failed { .. } => "failed",
        TaskOutcome::Aborted { .. } => "aborted",
        TaskOutcome::Orphaned { .. } => "orphaned",
        TaskOutcome::Faulted { .. } => "faulted",
    }
}
/// A terminal transition for a task that finished with `outcome`.
pub fn terminal<S, R>(outcome: TaskOutcome<R>) -> Option<NextTaskState<S, R>> {
    Some(NextTaskState::Terminal { outcome })
}
pub fn failure(message: &str) -> TaskOutcomeError {
    TaskOutcomeError {
        message: message.into(),
        detail: None,
    }
}

/// The resolved native agent of a conversation.
pub async fn agent_of(
    conversation: &Conversation,
) -> Result<Arc<Agent<NativeTool, NativeSection>>> {
    conversation.agent(&ctx()).await?.typed()
}
pub fn tool_names(agent: &Arc<Agent<NativeTool, NativeSection>>) -> Vec<String> {
    agent
        .tools
        .iter()
        .map(|tool| tool.declaration.name.to_string())
        .collect()
}
pub fn extension_names(agent: &Arc<Agent<NativeTool, NativeSection>>) -> Vec<String> {
    agent
        .extensions
        .iter()
        .map(|extension| extension.name.to_string())
        .collect()
}
pub fn change(value: serde_json::Value) -> JsonObject {
    object(&value.to_string())
}

/// The `pi.live` document of a conversation view, as `serde_json`.
pub fn live_of(view: &ConversationView) -> serde_json::Value {
    view.docs
        .get(&JsonString::from("pi.live"))
        .map_or(serde_json::Value::Null, |live| sj(live))
}
