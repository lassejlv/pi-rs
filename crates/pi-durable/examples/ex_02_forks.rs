//! Fork a conversation and watch its rewindable document diverge from the parent.
//! Port of upstream test/examples/02-forks.ts.
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}
impl DocumentState for Notes {}

async fn write_note(
    session: &Session,
    notes: &RewindableConversationDocToken<Notes>,
    id: ConversationId,
    data: &'static str,
    text: &'static str,
    context: &Context,
) -> Result<EntryRecord> {
    let notes = notes.clone();
    session
        .commit(
            move |tx| async move {
                let mut draft = EntryDraft::new("note");
                draft.data = Some(JsonValue::from(data));
                let entry = tx.append_entry(id, draft).await?;
                tx.doc(&notes, id).await?.update(|n| {
                    n.text = text.into();
                    Ok(())
                })?;
                Ok(entry)
            },
            context,
        )
        .await
}

async fn set_text(
    session: &Session,
    notes: &RewindableConversationDocToken<Notes>,
    id: ConversationId,
    text: &'static str,
    context: &Context,
) -> Result<()> {
    let notes = notes.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&notes, id).await?.update(|n| {
                    n.text = text.into();
                    Ok(())
                })
            },
            context,
        )
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let notes: RewindableConversationDocToken<Notes> = define_doc(DocDefinition::new(
        "example.notes",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        || {
            Ok(Notes {
                text: String::new(),
            })
        },
        |_, _, _| false,
    ))?;
    let chat = session
        .commit(
            |tx| async move {
                tx.create_conversation(ConversationOwnership::Ownerless)
                    .await
            },
            &context,
        )
        .await?;
    let first = write_note(&session, &notes, chat.id, "hello", "after hello", &context).await?;
    write_note(
        &session,
        &notes,
        chat.id,
        "goodbye",
        "after goodbye",
        &context,
    )
    .await?;

    let first_id = first.id;
    let branch = session
        .commit(
            move |tx| async move {
                tx.fork_conversation(chat.id, first_id, ConversationOwnership::Ownerless)
                    .await
            },
            &context,
        )
        .await?;

    let branch_id = branch.id;
    let page = session
        .commit(
            move |tx| async move {
                tx.scan_entries(
                    EntryQuery {
                        conversation_id: branch_id,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: Some(ScanOrder::Ascending),
                    },
                    10,
                    None,
                )
                .await
            },
            &context,
        )
        .await?;
    let transcript: Vec<Option<String>> = page
        .items
        .iter()
        .map(|e| {
            e.data
                .as_ref()
                .and_then(|d| d.to_utf8().map(|s| s.to_string()))
        })
        .collect();
    println!("fork transcript: {transcript:?}");
    assert_eq!(transcript, vec![Some("hello".to_string())]);

    let fork_notes = session
        .snapshot(&notes, branch.id, &context)
        .await?
        .unwrap();
    println!("fork notes: {fork_notes:?}");
    assert_eq!(fork_notes.text, "after hello");

    set_text(
        &session,
        &notes,
        branch.id,
        "changed only in the fork",
        &context,
    )
    .await?;
    let fork_after = session
        .snapshot(&notes, branch.id, &context)
        .await?
        .unwrap();
    let parent_after = session.snapshot(&notes, chat.id, &context).await?.unwrap();
    println!("fork notes after edit: {fork_after:?}");
    println!("parent notes after edit: {parent_after:?}");
    assert_eq!(fork_after.text, "changed only in the fork");
    assert_eq!(parent_after.text, "after goodbye");
    session.close(&context).await
}
