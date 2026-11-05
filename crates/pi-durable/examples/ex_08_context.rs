//! Transcript history and model context: heads, edits, ordering and synthesized results.
//! Port of upstream test/examples/08-context.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_durable::*;
use std::sync::Arc;
use support::*;

fn assistant(text: &str, calls: &[&str], stop_reason: Option<&str>) -> JsonValue {
    let calls: Vec<String> = calls
        .iter()
        .map(|id| format!(r#"{{"type":"toolCall","id":"{id}","name":"read","arguments":{{}}}}"#))
        .collect();
    let mut content = vec![format!(r#"{{"type":"text","text":"{text}"}}"#)];
    content.extend(calls.iter().cloned());
    let stop = stop_reason.unwrap_or(if calls.is_empty() { "stop" } else { "toolUse" });
    json(&format!(
        r#"{{"role":"assistant","content":[{}],"api":"example","provider":"example","model":"example",
        "usage":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
        "cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}},
        "stopReason":"{stop}","timestamp":2}}"#,
        content.join(",")
    ))
}
fn tool_result(id: &str) -> JsonValue {
    json(&format!(
        r#"{{"role":"toolResult","toolCallId":"{id}","toolName":"read",
        "content":[{{"type":"text","text":"file {id}"}}],"isError":false,"timestamp":3}}"#
    ))
}
fn user(text: &str, timestamp: u32) -> JsonValue {
    json(&format!(
        r#"{{"role":"user","content":"{text}","timestamp":{timestamp}}}"#
    ))
}

async fn say(
    transcript: &Conversation,
    context: &Context,
    kind: &'static str,
    model: Vec<JsonValue>,
) -> Result<EntryRecord> {
    let id = transcript.id;
    transcript
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new(kind);
                draft.model = Some(model);
                tx.append_entry(id, draft).await
            },
            context,
        )
        .await
}
fn shown(view: &harness::context::ContextView) -> Vec<String> {
    view.messages.iter().map(show).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(
            create_registry::<NativeTool, NativeSection>()?,
            pi_ai::models::Models::default(),
        ),
        &context,
    )
    .await?;
    let transcript = harness
        .create_conversation(
            ConversationCreateOptions::new(ConversationOwnership::Ownerless),
            &context,
        )
        .await?;
    let id = transcript.id;

    let question = say(
        &transcript,
        &context,
        "message",
        vec![user("read a and b", 1)],
    )
    .await?;
    // Stored, never sent.
    say(
        &transcript,
        &context,
        "message",
        vec![assistant("I crashed", &[], Some("aborted"))],
    )
    .await?;
    let calls = say(
        &transcript,
        &context,
        "message",
        vec![assistant("reading", &["a", "b"], None)],
    )
    .await?;
    // Results finish out of order.
    say(&transcript, &context, "message", vec![tool_result("b")]).await?;
    say(
        &transcript,
        &context,
        "pi.system",
        vec![json(
            r#"{"role":"system","content":"","sections":{"cwd":"<cwd>/repo</cwd>"},"timestamp":4}"#,
        )],
    )
    .await?;
    say(&transcript, &context, "message", vec![tool_result("a")]).await?;
    say(
        &transcript,
        &context,
        "message",
        vec![assistant("a and b look fine", &[], None)],
    )
    .await?;
    let target = question.id;
    transcript
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("edit");
                draft.data = Some(JsonValue::from("user fixed a typo"));
                draft.edits = Some(vec![ContextEdit::Replace {
                    target,
                    messages: vec![user("read files a and b", 1)],
                }]);
                tx.append_entry(id, draft).await
            },
            &context,
        )
        .await?;
    transcript
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("note");
                draft.data = Some(JsonValue::from("display only"));
                tx.append_entry(id, draft).await
            },
            &context,
        )
        .await?;

    let view = transcript.context(None, &context).await?;
    let kinds: Vec<String> = view.entries.iter().map(|e| e.kind.to_string()).collect();
    println!("raw active entries: {kinds:?}");
    let request = shown(&view);
    println!("request messages: {request:#?}");
    assert_eq!(kinds.len(), 9);
    // The aborted answer is dropped, the edit replaced the question, results follow their call in
    // call order (a before b) even though b finished first, and the system message leads the request.
    assert_eq!(
        request,
        [
            r#"system: {"cwd":"<cwd>/repo</cwd>"}"#,
            "user: read files a and b",
            "assistant: reading call(a) call(b)",
            "result(a)",
            "result(b)",
            "assistant: a and b look fine",
        ]
    );

    // A fork at the tool call has no results yet; context() fills them in.
    let cut = transcript
        .fork(
            calls.id,
            ConversationCreateOptions::new(ConversationOwnership::Ownerless),
            &context,
        )
        .await?;
    let forked = shown(&cut.context(None, &context).await?);
    println!("fork messages: {forked:#?}");
    assert_eq!(forked.len(), 4);
    assert_eq!(
        forked[..2],
        ["user: read a and b", "assistant: reading call(a) call(b)"]
    );
    assert_eq!(forked[2..], ["result(a) error", "result(b) error"]);

    // A headed summary replaces everything before the entry it points at.
    transcript
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("summary");
                draft.head = Some(EntryHead::ThisEntry);
                draft.model = Some(vec![user("Summary: a and b are fine.", 5)]);
                tx.append_entry(id, draft).await
            },
            &context,
        )
        .await?;
    let view = transcript.context(None, &context).await?;
    let head_kind = view.head.as_ref().map(|h| h.kind.to_string());
    println!("after summary: {head_kind:?} {:?}", shown(&view));
    assert_eq!(head_kind.as_deref(), Some("summary"));
    assert_eq!(shown(&view), ["user: Summary: a and b are fine."]);

    // entries() pages the stored transcript, newest first; nothing is deleted by heads or edits.
    let history = transcript
        .entries(ConversationEntryQuery::default(), 3, None, &context)
        .await?;
    let newest: Vec<String> = history.items.iter().map(|e| e.kind.to_string()).collect();
    println!(
        "newest stored entries: {newest:?} more: {}",
        history.next.is_some()
    );
    assert_eq!(newest, ["summary", "note", "edit"]);
    assert!(history.next.is_some());

    harness.close(&context).await
}
