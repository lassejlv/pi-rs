//! A Harness conversation in the browser over IndexedDbStorage, with the faux
//! provider: a durable input submission, a tool call and its result, a streamed
//! answer paced by real `setTimeout` timers, close, reopen and the transcript.
use crate::context;
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
        faux_text, faux_tool_call,
    },
};
use pi_durable::{
    harness::tool::text_content,
    storage::indexeddb::{IndexedDbStorage, web::WebIdbFactory},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wasm_bindgen::prelude::*;

fn json(source: &str) -> JsonValue {
    pi_chord::json::parse(source).expect("test JSON")
}
fn check(ok: bool, what: impl Into<String>) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::message(what)) }
}
fn step(what: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("harness: {what}")));
}

/// `lookup { path }` answers `"found <path>"` and counts its calls.
fn registry(calls: Arc<AtomicUsize>) -> Result<Registry<NativeTool, NativeSection>> {
    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut extension = define_extension(Extension::new("lookup"));
    extension.tools.push(Arc::new(NativeTool::new(
        ToolDeclaration::new(
            "lookup",
            "Look a path up",
            json(r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#),
        ),
        Arc::new(move |args, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            let path = args.get(&JsonString::from("path")).and_then(JsonValue::to_utf8).unwrap_or_default();
            Box::pin(async move {
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![text_content(format!("found {path}").into())])),
                    ..Default::default()
                })
            })
        }),
    )));
    registry.install(Arc::new(extension))?;
    Ok(registry)
}
fn agent() -> ConversationOptions {
    ConversationOptions {
        agent: Some(
            JsonObject::try_from_value(json(r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#))
                .expect("agent object"),
        ),
        ..Default::default()
    }
}
fn kinds(entries: &Page<EntryRecord>) -> Vec<String> {
    entries.items.iter().rev().map(|entry| entry.kind.to_string()).collect()
}

async fn turn() -> Result<String> {
    let factory = WebIdbFactory::global()?;
    let name = "harness-turn";
    factory.delete(name).await?;
    // Finite pacing: the faux stream waits on real timers between chunks.
    let faux = faux_provider(RegisterFauxProviderOptions {
        tokens_per_second: Some(400.0),
        ..Default::default()
    });
    let models = Models::default();
    models.set_provider(faux.provider.clone());
    faux.set_responses(vec![
        faux_assistant_message(
            vec![faux_tool_call("lookup", serde_json::from_str(r#"{"path":"notes.txt"}"#).unwrap(), Some("call-1"))],
            FauxMessageOptions { stop_reason: Some(pi_ai::StopReason::ToolUse), ..Default::default() },
        )
        .into(),
        faux_assistant_message(vec![faux_text("The notes are in notes.txt.")], Default::default()).into(),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(IndexedDbStorage::open_with(&factory, name).await?);
    let harness = Harness::open(storage, HarnessOptions::new(registry(calls.clone())?, models.clone()), &context()).await?;
    step("opened");
    let root = harness.root(agent(), &context()).await?;
    let submission = root.submit(SubmissionDraft::input("Where are my notes?"), &context()).await?;
    step("submitted");
    let settled = submission.wait(&context()).await?;
    step("answered");
    check(settled.state.status() == SubmissionStatus::Done, format!("submission settled as {:?}", settled.state.status()))?;
    check(calls.load(Ordering::SeqCst) == 1, "the tool ran once")?;
    let before = root.entries(ConversationEntryQuery::default(), 20, None, &context()).await?;
    let before_kinds = kinds(&before);
    let root_id = root.id;
    harness.close(&context()).await?;
    step("closed");

    // Reopen from IndexedDB alone: the transcript and the settled receipt are durable.
    let storage = Arc::new(IndexedDbStorage::open_with(&factory, name).await?);
    let reopened = Harness::open(storage.clone(), HarnessOptions::new(registry(calls.clone())?, models), &context()).await?;
    let root = reopened.root(agent(), &context()).await?;
    check(root.id == root_id, "the same root conversation")?;
    let after = root.entries(ConversationEntryQuery::default(), 20, None, &context()).await?;
    check(kinds(&after) == before_kinds, format!("transcript after reopen {:?} vs {:?}", kinds(&after), before_kinds))?;
    let receipt = storage.submission(submission.id, &context()).await?;
    check(
        receipt.as_ref().is_some_and(|r| r.state.status() == SubmissionStatus::Done),
        "the durable submission receipt is Done",
    )?;
    check(calls.load(Ordering::SeqCst) == 1, "no tool re-ran after reopen")?;
    let answer = after
        .items
        .iter()
        .find(|entry| entry.kind.to_string() == "pi.assistant")
        .and_then(|entry| serde_json::to_string(&entry.model).ok())
        .unwrap_or_default();
    check(answer.contains("The notes are in notes.txt."), format!("final answer in transcript: {answer}"))?;
    reopened.close(&context()).await?;
    step("reopened and closed");
    Ok(serde_json::json!({ "transcript": before_kinds, "toolCalls": 1 }).to_string())
}

#[wasm_bindgen]
pub async fn harness_turn() -> std::result::Result<String, JsValue> {
    turn().await.map_err(|e| JsValue::from_str(&e.to_string()))
}
