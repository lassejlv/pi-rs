//! A task owns a child conversation; a registry document in the main conversation records it.
//! Port of upstream test/examples/03-owned-conversations.ts.
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SupervisorState {
    phase: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRef {
    conversation_id: ConversationId,
    request_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AgentRegistry {
    agents: BTreeMap<String, AgentRef>,
}
impl DocumentState for AgentRegistry {}

fn idle() -> PhaseHandler<(), SupervisorState, (), ()> {
    Arc::new(|_, _, _| Box::pin(async { Ok(()) }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let session = Session::new(Arc::new(MemoryStorage::new()), SessionOptions::default());

    // A task that owns conversations. Its phases do nothing: the example only records ownership.
    let supervisor: Task<(), SupervisorState, (), ()> = define_task(TaskDefinition {
        name: "example.supervisor".into(),
        version: 1,
        initial: Arc::new(|_| {
            Ok(SupervisorState {
                phase: "ready".into(),
            })
        }),
        phases: [(JsonString::from("ready"), idle())].into_iter().collect(),
        abort: idle(),
        migrate: None,
        hooks: None,
    });

    // Latest-history document; a fork starts from the initial value.
    let registry: LatestConversationDocToken<AgentRegistry> = define_doc(DocDefinition::new(
        "example.agent-registry",
        1,
        LatestConversationScope {
            fork: LatestFork::Initial,
        },
        || {
            Ok(AgentRegistry {
                agents: BTreeMap::new(),
            })
        },
        |_, _, _| false,
    ))?;

    let main = session
        .commit(
            |tx| async move {
                tx.create_conversation(ConversationOwnership::Ownerless)
                    .await
            },
            &context,
        )
        .await?;
    let main_id = main.id;

    let (supervisor_id, child) = {
        let (supervisor, registry) = (supervisor.clone(), registry.clone());
        session
            .commit(
                move |tx| async move {
                    let supervisor_id = tx
                        .create_task(
                            &supervisor,
                            (),
                            TaskOptions {
                                conversation_id: Some(main_id),
                                background: true,
                                ..TaskOptions::new(TaskOwnership::Conversation)
                            },
                        )
                        .await?
                        .untyped();
                    let child = tx
                        .create_conversation(ConversationOwnership::Task(supervisor_id))
                        .await?;
                    tx.doc(&registry, main_id).await?.update(|r| {
                        r.agents.insert(
                            "researcher".into(),
                            AgentRef {
                                conversation_id: child.id,
                                request_id: format!("researcher:first-message:{supervisor_id:?}"),
                            },
                        );
                        Ok(())
                    })?;
                    Ok((supervisor_id, child))
                },
                &context,
            )
            .await?
    };
    println!("supervisor task: {supervisor_id:?}");
    println!("child conversation: {child:?}");
    let stored = session
        .snapshot(&registry, main_id, &context)
        .await?
        .unwrap();
    println!("registry: {stored:?}");

    let owner = child
        .owner
        .as_ref()
        .expect("child conversation is task-owned");
    assert_eq!(owner.task_id, supervisor_id);
    assert_eq!(owner.conversation_id, main_id);
    assert_eq!(stored.agents["researcher"].conversation_id, child.id);
    assert!(
        stored.agents["researcher"]
            .request_id
            .starts_with("researcher:first-message:")
    );

    let task = session
        .commit(
            move |tx| async move { tx.task(supervisor_id).await },
            &context,
        )
        .await?
        .expect("supervisor task is stored");
    assert!(task.info.background);
    assert_eq!(task.info.conversation_id, main_id);
    session.close(&context).await
}
