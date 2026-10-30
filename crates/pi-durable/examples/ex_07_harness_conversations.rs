//! Conversation handles, typed entries, created conversations, and forks.
//! Port of upstream test/examples/07-harness-conversations.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use support::*;

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    from: String,
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
    let root = harness
        .root(ConversationOptions::default(), &context)
        .await?;
    root.configure(obj(r#"{"thinkingLevel":"high"}"#), &context)
        .await?;

    // Conversation handles bind commits to their conversation. An entry token types an entry kind.
    let message: Entry<Payload> = define_entry("message")?;
    let root_id = root.id;
    let token = message.clone();
    let hello = root
        .commit(
            move |tx| async move {
                token
                    .append(
                        &tx,
                        root_id,
                        &Payload {
                            from: "example".into(),
                        },
                        Some(vec![json(
                            r#"{"role":"user","content":"hello","timestamp":1}"#,
                        )]),
                    )
                    .await
            },
            &context,
        )
        .await?;
    let from = message.data(&hello)?.expect("typed data").from;
    println!("typed entry: {} {from}", message.is(Some(&hello)));
    assert!(message.is(Some(&hello)));
    assert_eq!(from, "example");

    // create_conversation() and fork() apply `agent` and run `init` in the creating commit.
    // A fork starts with the agent the parent had at the fork entry.
    let helper = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(obj(r#"{"thinkingLevel":"minimal"}"#)),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;
    let retry = root
        .fork(
            hello.id,
            ConversationCreateOptions::new(ConversationOwnership::Ownerless),
            &context,
        )
        .await?;
    let helper_thinking = helper.agent(&context).await?.thinking_level().to_string();
    let fork_thinking = retry.agent(&context).await?.thinking_level().to_string();
    println!("helper thinking: {helper_thinking}");
    println!("fork thinking: {fork_thinking}");
    assert_eq!(helper_thinking, "minimal");
    assert_eq!(fork_thinking, "high");

    let found = harness.conversation(retry.id, &context).await?;
    println!("lookup: {}", found.as_ref().map(|c| c.id) == Some(retry.id));
    assert_eq!(found.map(|c| c.id), Some(retry.id));

    harness.close(&context).await
}
