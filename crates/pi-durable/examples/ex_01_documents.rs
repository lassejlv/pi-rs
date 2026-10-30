//! Store document state next to transcript entries.
//! Port of upstream test/examples/01-documents.ts.
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}
impl DocumentState for Notes {}

/// A rewindable conversation document; a fork starts with the value at the fork entry.
fn notes() -> Result<RewindableConversationDocToken<Notes>> {
    define_doc(DocDefinition::new(
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
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let notes = notes()?;
    let chat = session
        .commit(
            |tx| async move {
                tx.create_conversation(ConversationOwnership::Ownerless)
                    .await
            },
            &context,
        )
        .await?;
    let id = chat.id;
    let mut entries = Vec::new();
    for (data, text) in [("hello", "after hello"), ("goodbye", "after goodbye")] {
        let token = notes.clone();
        let entry = session
            .commit(
                move |tx| async move {
                    let mut draft = EntryDraft::new("note");
                    draft.data = Some(JsonValue::from(data));
                    let entry = tx.append_entry(id, draft).await?;
                    tx.doc(&token, id).await?.update(|n| {
                        n.text = text.into();
                        Ok(())
                    })?;
                    Ok(entry)
                },
                &context,
            )
            .await?;
        entries.push(entry);
    }
    let latest = session.snapshot(&notes, id, &context).await?.unwrap();
    let at_first = session
        .snapshot_as_of(&notes, id, entries[0].id, &context)
        .await?
        .unwrap();
    let at_second = session
        .snapshot_as_of(&notes, id, entries[1].id, &context)
        .await?
        .unwrap();
    println!("latest notes: {latest:?}");
    println!("notes at first entry: {at_first:?}");
    println!("notes at second entry: {at_second:?}");
    assert_eq!(latest.text, "after goodbye");
    assert_eq!(at_first.text, "after hello");
    assert_eq!(at_second.text, "after goodbye");
    session.close(&context).await
}
