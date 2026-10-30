//! An extension that keeps its own per-conversation document, written by a tool and shown by a
//! prompt section. Port of upstream test/examples/10-extension-state.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_ai::providers::faux::*;
use pi_durable::harness::tool::text_content;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use support::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Todos {
    items: Vec<String>,
}
impl DocumentState for Todos {}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    // A todo list per conversation. Rewindable with `asOf`, so a fork starts with the todos its
    // parent had at the fork entry.
    let todos: RewindableConversationDocToken<Todos> = define_doc(DocDefinition::new(
        "example.todos",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        || Ok(Todos { items: Vec::new() }),
        |_, _, _| false,
    ))?;

    // The tool writes the document; the section shows it to the model before every request.
    // tx.doc() creates the document on first write, and the section treats a missing document as
    // an empty list.
    let tool_todos = todos.clone();
    let tool = NativeTool::new(
        ToolDeclaration::new(
            "todo",
            "Add an item to your todo list",
            json(
                r#"{"type":"object","properties":{"item":{"type":"string"}},"required":["item"]}"#,
            ),
        ),
        Arc::new(move |args, api, call_context| {
            let todos = tool_todos.clone();
            let item = text(args.get(&JsonString::from("item")).expect("item argument"));
            Box::pin(async move {
                let conversation = api.conversation_id();
                let added = item.clone();
                api.commit(
                    move |tx| async move {
                        tx.doc(&todos, conversation).await?.update(|t| {
                            t.items.push(added);
                            Ok(())
                        })
                    },
                    &call_context,
                )
                .await?;
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![text_content(format!("added {item}").into())])),
                    ..Default::default()
                })
            })
        }),
    );
    let section_todos = todos.clone();
    let todo_section = section(
        "todos",
        Arc::new(move |input, render_context| {
            let todos = section_todos.clone();
            Box::pin(async move {
                let snapshot = input
                    .read
                    .snapshot(&todos, input.conversation_id, &render_context)
                    .await?;
                Ok(match snapshot {
                    Some(todos) if !todos.items.is_empty() => Some(todos.items.join("\n").into()),
                    _ => None,
                })
            })
        }),
        SectionOptions::default(),
    );
    let mut todo = define_extension(Ext::new("todo"));
    todo.tools.push(Arc::new(tool));
    todo.sections.push(Arc::new(todo_section));

    let (models, faux) = faux_models();
    let registry = create_registry::<NativeTool, NativeSection>()?;
    registry.install(Arc::new(todo))?;
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let root = harness.root(faux_agent(), &context).await?;

    faux.set_responses(vec![
        faux_assistant_message(
            vec![faux_tool_call(
                "todo",
                serde_json::from_str(r#"{"item":"fix the build"}"#).unwrap(),
                None,
            )],
            FauxMessageOptions {
                stop_reason: Some(pi_ai::StopReason::ToolUse),
                ..Default::default()
            },
        )
        .into(),
        faux_text_message("Noted.").into(),
        faux_text_message("Working on it.").into(),
    ]);
    root.submit(
        SubmissionDraft::input("Remember to fix the build."),
        &context,
    )
    .await?
    .wait(&context)
    .await?;
    let stored = harness.snapshot(&todos, root.id, &context).await?.unwrap();
    println!("todos: {stored:?}");
    assert_eq!(stored.items, ["fix the build"]);

    root.submit(SubmissionDraft::input("What is next?"), &context)
        .await?
        .wait(&context)
        .await?;
    let view = root.context(None, &context).await?;
    let systems: Vec<serde_json::Value> = view
        .messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .filter(|m| m["role"] == "system")
        .collect();
    println!("system messages: {systems:#?}");
    // The first system message only announced the todo tool; the next request's carries the list.
    assert_eq!(systems.len(), 2);
    assert_eq!(systems[0]["toolsAdded"][0]["name"], "todo");
    assert!(systems[0]["sections"]["todos"].is_null());
    assert!(systems[1]["sections"].to_string().contains("fix the build"));

    harness.close(&context).await
}
