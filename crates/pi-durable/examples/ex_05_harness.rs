//! Open a Harness over storage, create a root conversation with an agent and documents.
//! Port of upstream test/examples/05-harness.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use support::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}
impl DocumentState for Notes {}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let registry = create_registry::<NativeTool, NativeSection>()?;
    let mut files = define_extension(Ext::new("files"));
    files.tools.push(path_tool("read", "Read a file"));
    registry.install(Arc::new(files))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, pi_ai::models::Models::default()),
        &context,
    )
    .await?;

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

    // `init` runs in the same transaction that creates the conversation.
    let init_notes = notes.clone();
    let root = harness
        .root(
            ConversationOptions {
                agent: Some(obj(r#"{"thinkingLevel":"low"}"#)),
                init: Some(Arc::new(move |tx, id| {
                    let notes = init_notes.clone();
                    Box::pin(async move {
                        tx.doc(&notes, id).await?.update(|n| {
                            n.text = "root notes".into();
                            Ok(())
                        })
                    })
                })),
            },
            &context,
        )
        .await?;
    let stored_notes = harness.snapshot(&notes, root.id, &context).await?.unwrap();
    let stored_agent = harness
        .snapshot(&AGENT_DOC, root.id, &context)
        .await?
        .unwrap();
    println!("root: {:?} {stored_notes:?}", root.id);
    println!("stored agent: {}", stored_agent.0.value());
    assert_eq!(stored_notes.text, "root notes");
    assert_eq!(
        text(
            stored_agent
                .0
                .get(&JsonString::from("thinkingLevel"))
                .unwrap()
        ),
        "low"
    );

    let agent = root.agent(&context).await?;
    let extensions = extension_names(&agent)?;
    let tools = tool_names(&agent)?;
    println!(
        "resolved: {} {extensions:?} {tools:?}",
        agent.thinking_level()
    );
    assert_eq!(agent.thinking_level().to_string(), "low");
    assert_eq!(extensions, ["files"]);
    assert_eq!(tools, ["read"]);

    harness.close(&context).await
}
