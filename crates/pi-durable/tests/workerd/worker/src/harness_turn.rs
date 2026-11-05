//! A Harness conversation inside workerd over the Durable Object's own SQLite,
//! with the faux provider: the scenario `tests/indexeddb_browser` runs in Chrome.
//! `harnessTurnBegin` submits an input whose answer calls a tool and streams on
//! real timers; the test evicts the object; `harnessTurnResume` reopens the
//! storage in a new instance and checks the transcript and the durable receipt.
use pi_ai::{
    models::Models,
    providers::faux::{
        FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
        faux_text, faux_tool_call,
    },
};
use pi_durable::{
    harness::tool::text_content,
    storage::sqlite::{SqliteStorageOptions, cloudflare_js::open_js_durable_object_sqlite_storage},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wasm_bindgen::prelude::*;

const PREFIX: &str = "harness_turn_";

fn json(source: &str) -> JsonValue {
    pi_chord::json::parse(source).expect("test JSON")
}
fn fail(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
fn check(ok: bool, what: impl Into<String>) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::message(what)) }
}
fn context() -> Context {
    Context::default()
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
fn models() -> Models {
    // Finite pacing: the faux stream waits on workerd timers between chunks.
    let faux = faux_provider(RegisterFauxProviderOptions { tokens_per_second: Some(400.0), ..Default::default() });
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
    models
}
fn agent() -> ConversationOptions {
    ConversationOptions {
        agent: Some(JsonObject::try_from_value(json(r#"{"model":{"provider":"faux","modelId":"faux-1"}}"#)).expect("agent")),
        ..Default::default()
    }
}
fn kinds(entries: &Page<EntryRecord>) -> Vec<String> {
    entries.items.iter().rev().map(|entry| entry.kind.to_string()).collect()
}
async fn open(storage: JsValue, calls: Arc<AtomicUsize>) -> Result<(Arc<dyn Storage>, Harness)> {
    let sql: Arc<dyn Storage> = Arc::new(
        open_js_durable_object_sqlite_storage(storage, SqliteStorageOptions { prefix: Some(PREFIX.into()) }).await?,
    );
    let harness = Harness::open(sql.clone(), HarnessOptions::new(registry(calls)?, models()), &context()).await?;
    Ok((sql, harness))
}

async fn begin(storage: JsValue) -> Result<String> {
    let calls = Arc::new(AtomicUsize::new(0));
    let (_, harness) = open(storage, calls.clone()).await?;
    let root = harness.root(agent(), &context()).await?;
    let submission = root.submit(SubmissionDraft::input("Where are my notes?"), &context()).await?;
    let settled = submission.wait(&context()).await?;
    check(settled.state.status() == SubmissionStatus::Done, format!("submission settled as {:?}", settled.state.status()))?;
    check(calls.load(Ordering::SeqCst) == 1, "the tool ran once")?;
    let entries = root.entries(ConversationEntryQuery::default(), 20, None, &context()).await?;
    let result = serde_json::json!({
        "root": root.id.get(),
        "submission": submission.id.get(),
        "transcript": kinds(&entries),
    });
    harness.close(&context()).await?;
    Ok(result.to_string())
}

async fn resume(storage: JsValue, root_id: f64, submission_id: f64, transcript: Vec<String>) -> Result<String> {
    let calls = Arc::new(AtomicUsize::new(0));
    let (sql, harness) = open(storage, calls.clone()).await?;
    let root = harness.root(agent(), &context()).await?;
    check(root.id.get() as f64 == root_id, "the same root conversation")?;
    let entries = root.entries(ConversationEntryQuery::default(), 20, None, &context()).await?;
    check(kinds(&entries) == transcript, format!("transcript after eviction {:?} vs {transcript:?}", kinds(&entries)))?;
    let receipt = sql.submission(SubmissionId::from_raw(submission_id as i64), &context()).await?;
    check(receipt.is_some_and(|r| r.state.status() == SubmissionStatus::Done), "the durable submission receipt is Done")?;
    let answer = entries
        .items
        .iter()
        .find(|entry| entry.kind.to_string() == "pi.assistant")
        .and_then(|entry| serde_json::to_string(&entry.model).ok())
        .unwrap_or_default();
    check(answer.contains("The notes are in notes.txt."), format!("final answer in transcript: {answer}"))?;
    check(calls.load(Ordering::SeqCst) == 0, "no tool re-ran after eviction")?;
    harness.close(&context()).await?;
    Ok(transcript.join(" > "))
}

#[wasm_bindgen(js_name = harnessTurnBegin)]
pub async fn harness_turn_begin(storage: JsValue) -> std::result::Result<String, JsValue> {
    begin(storage).await.map_err(fail)
}
#[wasm_bindgen(js_name = harnessTurnResume)]
pub async fn harness_turn_resume(
    storage: JsValue,
    root: f64,
    submission: f64,
    transcript: Vec<String>,
) -> std::result::Result<String, JsValue> {
    resume(storage, root, submission, transcript).await.map_err(fail)
}
