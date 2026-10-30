//! A Session stores conversations, transcript entries, tasks, and documents.
//! Port of upstream test/examples/00-conversation.ts.
use pi_durable::*;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // MemoryStorage keeps everything in memory; other storage backends keep it on disk.
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());
    // Every Session call takes a context, which is used for cancellation.
    let context = Context::default();
    // All writes happen inside session.commit(); they are saved together or discarded.
    let standalone = session
        .commit(
            |tx| async move {
                tx.create_conversation(ConversationOwnership::Ownerless)
                    .await
            },
            &context,
        )
        .await?;
    println!("standalone conversation: {standalone:?}");
    assert!(
        standalone.owner.is_none(),
        "ownerless conversation has no owner"
    );
    assert!(standalone.parent.is_none());
    session.close(&context).await
}
