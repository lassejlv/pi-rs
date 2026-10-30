//! Plan mode: the user switches a conversation into a read-only planning mode, the agent writes a plan into the
//! extension's own document, and switching back restores the full tool set.
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_durable::harness::tool::ToolControl;
use pi_durable::tools::coding_tools;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, LazyLock};
use support::*;

// ─── Product code: the plan extension ───────────────────────────────────────

// The current plan of a conversation. A fork keeps the plan it had at the fork entry.
#[derive(Clone, Default, Serialize, Deserialize)]
struct PlanState {
    steps: Vec<String>,
}
impl DocumentState for PlanState {}
static PLAN_DOC: LazyLock<RewindableConversationDocToken<PlanState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "app.plan",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        || Ok(PlanState::default()),
        |_, _, _| false,
    ))
    .unwrap()
});

fn plan_extension() -> Ext {
    let submit_plan = tool(
        "submit_plan",
        "Submit the plan as a list of steps.",
        r#"{"type":"object","properties":{"steps":{"type":"array","items":{"type":"string"}}},"required":["steps"]}"#,
        |args, api, context| {
            Box::pin(async move {
                let steps: Vec<String> = sj(args.value())["steps"]
                    .as_array()
                    .map(|steps| {
                        steps
                            .iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let conversation = api.conversation_id();
                api.commit(
                    move |tx| async move {
                        tx.doc(&PLAN_DOC, conversation).await?.update(|plan| {
                            plan.steps = steps;
                            Ok(())
                        })
                    },
                    &context,
                )
                .await?;
                let mut result = text_result("Plan submitted.");
                result.control = Some(ToolControl {
                    terminate: Some(true),
                    ..Default::default()
                });
                Ok(result)
            })
        },
    );
    let mut extension = Ext::new("plan");
    extension.tools.push(Arc::new(submit_plan));
    extension.sections.push(Arc::new(static_section(
        "plan_mode",
        "You are in plan mode. Read the code, then call submit_plan. Change nothing.",
    )));
    extension
}

// ─── Host setup ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    std::fs::write(directory.path().join("server.ts"), "app.listen(3000);\n")
        .map_err(|e| Error::message(e.to_string()))?;

    let (models, faux) = faux(fast());
    faux.set_responses(vec![
        call("read", json!({"path": "server.ts"}), None),
        call(
            "submit_plan",
            json!({"steps": ["Read PORT from the environment", "Default to 3000"]}),
            None,
        ),
        answer("Implementing step 1."),
    ]);

    let registry = registry();
    install(&registry, coding_tools());
    install(&registry, plan_extension());
    // Installed, but only the coding tools are selected by default: conversations opt into plan mode.
    let mut options = HarnessOptions::new(registry, models).with_settings(HarnessSettings {
        extensions: Some(vec!["coding-tools".into()]),
        ..Default::default()
    });
    options.env = Some(cwd_env(directory.path().to_path_buf()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;
    let tools = || async { Ok::<_, Error>(tool_names(&agent_of(&root).await?)) };

    // Plan mode is a change to the conversation's agent: select the plan extension and offer only reading and
    // submitting. Clearing both returns to the host's default selection and every tool.
    let before = tools().await?;
    println!("tools: {before:?}");
    check(before == ["read", "write", "edit", "bash"], "default tools")?;

    root.configure(
        change(json!({"extensions": {"add": [{"name": "plan"}]}, "tools": [{"name": "read"}, {"name": "submit_plan"}]})),
        &context,
    )
    .await?;
    let in_plan = tools().await?;
    println!("plan mode tools: {in_plan:?}");
    check(
        in_plan == ["read", "submit_plan"],
        "plan mode offers only reading and submitting",
    )?;
    root.submit(
        SubmissionDraft::input("Make the port configurable."),
        &context,
    )
    .await?
    .wait(&context)
    .await?;
    let plan = harness.snapshot(&PLAN_DOC, root.id, &context).await?;
    let steps = plan.map(|plan| plan.steps.clone()).unwrap_or_default();
    println!("plan: {steps:?}");
    check(
        steps == ["Read PORT from the environment", "Default to 3000"],
        "the plan was stored in the extension's document",
    )?;

    root.configure(change(json!({"extensions": null, "tools": null})), &context)
        .await?;
    let after = tools().await?;
    println!("tools again: {after:?}");
    check(after == before, "leaving plan mode restores every tool")?;
    root.submit(SubmissionDraft::input("Go ahead."), &context)
        .await?
        .wait(&context)
        .await?;

    // The model saw each switch as a system prompt change in its transcript.
    let view = root.context(None, &context).await?;
    let mut switches = Vec::new();
    for message in &view.messages {
        let message = sj(message);
        if message["role"] != "system" {
            continue;
        }
        let names = |key: &str| -> Vec<String> {
            message[key]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        // A null section value removes it.
        println!(
            "system: sections {} added {:?} removed {:?}",
            message["sections"],
            names("toolsAdded"),
            names("toolsRemoved")
        );
        switches.push((
            message["sections"].clone(),
            names("toolsAdded"),
            names("toolsRemoved"),
        ));
    }
    check(switches.len() == 2, "one system message per switch")?;
    let enter = &switches[0];
    check(
        enter.0.get("plan_mode").is_some_and(|s| s.is_string()),
        "entering added the plan_mode section",
    )?;
    check(
        enter.1 == ["read", "submit_plan"] && enter.2.is_empty(),
        "entering offered the plan tool set",
    )?;
    let leave = &switches[1];
    check(
        leave.0["plan_mode"].is_null(),
        "leaving removed the plan_mode section",
    )?;
    check(
        leave.1 == ["write", "edit", "bash"] && leave.2 == ["submit_plan"],
        "leaving swapped the tool set back",
    )?;
    harness.close(&context).await
}
