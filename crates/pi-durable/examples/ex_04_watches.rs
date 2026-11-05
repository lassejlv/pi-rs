//! Observe a document asynchronously with a watch.
//! Port of upstream test/examples/04-watches.ts.
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}
impl DocumentState for Notes {}

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
    let chat = {
        let notes = notes.clone();
        session
            .commit(
                move |tx| async move {
                    let conversation = tx
                        .create_conversation(ConversationOwnership::Ownerless)
                        .await?;
                    tx.doc(&notes, conversation.id).await?.update(|n| {
                        n.text = "first".into();
                        Ok(())
                    })?;
                    Ok(conversation)
                },
                &context,
            )
            .await?
    };

    let watch = session
        .watch_doc(&notes, chat.id, &context)
        .await?
        .ok_or_else(|| Error::message("notes are absent"))?;
    let baseline = watch.value()?.unwrap();
    println!("watch baseline: {baseline:?}");
    assert_eq!(baseline.text, "first");

    let (sender, delivered) = oneshot::channel();
    let sender = Arc::new(parking_lot::Mutex::new(Some(sender)));
    watch.start(move |frame| {
        let sender = sender.clone();
        async move {
            let value = frame.value.expect("document is present");
            println!("watch update: {value:?}");
            if let Some(sender) = sender.lock().take() {
                let _ = sender.send(value.text.clone());
            }
            Ok(())
        }
    })?;
    {
        let notes = notes.clone();
        let id = chat.id;
        session
            .commit(
                move |tx| async move {
                    tx.doc(&notes, id).await?.update(|n| {
                        n.text = "observed asynchronously".into();
                        Ok(())
                    })
                },
                &context,
            )
            .await?;
    }
    let observed = delivered
        .await
        .map_err(|_| Error::message("watch listener never ran"))?;
    assert_eq!(observed, "observed asynchronously");
    watch.stop();
    watch.closed().await;
    session.close(&context).await
}
