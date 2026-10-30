//! One sandbox per conversation: the app records each conversation's sandbox in its own document, and the Harness
//! environment function looks it up for every tool call. Here a sandbox is a directory; a hosted product would
//! return an ExecutionEnv that runs inside the conversation's container.
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_durable::tools::coding_tools;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, LazyLock};
use support::*;

// Which sandbox a conversation runs in. `fork: Initial`: a fork gets no sandbox until the app assigns one.
#[derive(Clone, Default, Serialize, Deserialize)]
struct SandboxState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}
impl DocumentState for SandboxState {}
static SANDBOX: LazyLock<LatestConversationDocToken<SandboxState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "app.sandbox",
        1,
        LatestConversationScope {
            fork: LatestFork::Initial,
        },
        || Ok(SandboxState::default()),
        |_, _, _| false,
    ))
    .unwrap()
});

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let (models, faux) = faux(fast());
    let note = |text: &str| call("write", json!({"path": "note.txt", "content": text}), None);
    faux.set_responses(vec![
        note("from alice"),
        answer("Saved."),
        note("from bob"),
        answer("Saved."),
        note("from carol"),
        answer("Could not save."),
    ]);

    let registry = registry();
    install(&registry, coding_tools());
    let mut options = HarnessOptions::new(registry, models);
    // Committed reads only; a conversation without a sandbox gets no environment, so its tools fail cleanly.
    options.env = Some(Arc::new(|target, env_context| {
        Box::pin(async move {
            let sandbox = target
                .read
                .snapshot(&SANDBOX, target.conversation_id, &env_context)
                .await?;
            match sandbox.and_then(|sandbox| sandbox.path.clone()) {
                Some(path) => Ok(Some(native_env(std::path::Path::new(&path))?)),
                None => Ok(None),
            }
        })
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;

    // Each user's conversation gets a fresh sandbox in the creating commit.
    let conversation_for = |user: &'static str| {
        let harness = harness.clone();
        async move {
            let directory = tempfile::Builder::new()
                .prefix(&format!("pi-durable-sandbox-{user}-"))
                .tempdir()
                .map_err(|e| Error::message(e.to_string()))?;
            let path = directory.path().to_string_lossy().into_owned();
            let init: ConversationInit = Arc::new(move |tx, id| {
                let path = path.clone();
                Box::pin(async move {
                    tx.doc(&SANDBOX, id).await?.update(|sandbox| {
                        sandbox.path = Some(path);
                        Ok(())
                    })
                })
            });
            let conversation = harness
                .create_conversation(
                    ConversationCreateOptions {
                        agent: Some(agent("faux-1", json!({}))),
                        init: Some(init),
                        ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
                    },
                    &ctx(),
                )
                .await?;
            Ok::<_, Error>((conversation, directory))
        }
    };
    let (alice, alice_dir) = conversation_for("alice").await?;
    let (bob, bob_dir) = conversation_for("bob").await?;
    alice
        .submit(SubmissionDraft::input("Leave a note."), &context)
        .await?
        .wait(&context)
        .await?;
    bob.submit(SubmissionDraft::input("Leave a note."), &context)
        .await?
        .wait(&context)
        .await?;
    let read = |dir: &tempfile::TempDir| {
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap_or_default()
    };
    println!("alice's sandbox: {}", read(&alice_dir));
    println!("bob's sandbox: {}", read(&bob_dir));
    check(
        read(&alice_dir) == "from alice",
        "alice's note landed in alice's sandbox",
    )?;
    check(
        read(&bob_dir) == "from bob",
        "bob's note landed in bob's sandbox",
    )?;
    check(
        !alice_dir.path().join("from bob").exists(),
        "sandboxes are separate",
    )?;

    // A conversation without a sandbox has no environment; its tool call fails cleanly.
    let carol = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(agent("faux-1", json!({}))),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;
    let settled = carol
        .submit(SubmissionDraft::input("Leave a note."), &context)
        .await?
        .wait(&context)
        .await?;
    check(
        status_of(&settled) == SubmissionStatus::Done,
        "the run still completes",
    )?;
    let failure = last_tool_result(&carol).await?;
    println!("carol's tool result: {failure}");
    let entries = transcript(&carol, 20).await?;
    let result = entries
        .iter()
        .find(|e| is_kind(e, "pi.tool-result"))
        .and_then(message)
        .unwrap();
    check(
        result["isError"] == true,
        "the missing environment is an error result, not a crash",
    )?;
    harness.close(&context).await
}
