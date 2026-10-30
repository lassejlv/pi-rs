//! System prompt sections and per-conversation instructions.
//! Port of upstream test/examples/14-system-prompt.ts.
#[path = "ex_support/mod.rs"]
mod support;
use pi_ai::providers::faux::*;
use pi_durable::env::{ExecutionEnv, NativeExecutionEnv};
use pi_durable::*;
use std::sync::Arc;
use support::*;

fn section_extension(name: &str, sections: Vec<NativeSection>) -> Arc<Ext> {
    let mut extension = define_extension(Ext::new(name));
    extension.sections = sections.into_iter().map(Arc::new).collect();
    Arc::new(extension)
}

/// Every `pi.system` entry of a conversation, oldest first, as wire JSON.
async fn system_entries(
    conversation: &Conversation,
    context: &Context,
) -> Result<Vec<serde_json::Value>> {
    let page = conversation
        .entries(ConversationEntryQuery::default(), 20, None, context)
        .await?;
    Ok(page
        .items
        .iter()
        .rev()
        .filter(|entry| SYSTEM_ENTRY.is(Some(entry)))
        .flat_map(|entry| entry.model.clone().unwrap_or_default())
        .map(|message| serde_json::to_value(&message).expect("message serializes"))
        .collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = Context::default();
    let (models, faux) = faux_models();
    faux.set_responses(vec![
        faux_text_message("Done.").into(),
        faux_text_message("Done.").into(),
        faux_text_message("Done.").into(),
    ]);

    // Before each model request, the sections of the conversation's selected extensions render
    // the desired prompt, and only the difference to what the model already saw is appended to
    // the transcript as a `pi.system` entry. `tag: false` sends the text as is; by default it is
    // wrapped in <key>...</key>. Sections see the environment built for this request.
    let coding = section_extension(
        "coding",
        vec![
            section(
                "preamble",
                Arc::new(|_, _| Box::pin(async { Ok(Some("You are a coding agent.".into())) })),
                SectionOptions { tag: Some(false) },
            ),
            section(
                "cwd",
                Arc::new(|input, _| {
                    Box::pin(async move {
                        Ok(input
                            .env
                            .as_ref()
                            .map(|env| env.cwd().to_string_lossy().to_string().into()))
                    })
                }),
                SectionOptions::default(),
            ),
        ],
    );
    let agents_md = section_extension(
        "agents-md",
        vec![section(
            "agents_md",
            Arc::new(|_, _| {
                Box::pin(async { Ok(Some("Run npm run check after changes.".into())) })
            }),
            SectionOptions::default(),
        )],
    );
    // Another extension decorates a section by key without replacing it.
    let mut terse = define_extension(Ext::new("terse"));
    terse
        .wraps
        .push(wrap_section("preamble", |preamble: Arc<NativeSection>| {
            let inner = preamble.render.clone();
            Ok(Arc::new(NativeSection {
                key: preamble.key.clone(),
                tagged: preamble.tagged,
                render: Arc::new(move |input, context| {
                    let rendered = inner(input, context);
                    Box::pin(async move {
                        Ok(rendered
                            .await?
                            .map(|text| format!("{text} Be terse.").into()))
                    })
                }),
            }))
        }));

    let registry = create_registry::<NativeTool, NativeSection>()?;
    registry.install(coding)?;
    registry.install(agents_md)?;
    registry.install(Arc::new(terse))?;
    let mut options = HarnessOptions::new(registry, models);
    // The environment follows each conversation's agent `cwd`.
    options.env = Some(Arc::new(|target, _| {
        let cwd = target
            .cwd
            .map(|cwd| cwd.to_string())
            .unwrap_or_else(|| "/".into());
        Box::pin(async move {
            Ok(Some(Arc::new(
                NativeExecutionEnv::new(cwd).map_err(|error| Error::message(error.to_string()))?,
            ) as Arc<dyn ExecutionEnv>))
        })
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let model = r#""model":{"provider":"faux","modelId":"faux-1"}"#;
    let root = harness
        .root(
            ConversationOptions {
                agent: Some(obj(&format!(r#"{{{model},"cwd":"/repo"}}"#))),
                ..Default::default()
            },
            &context,
        )
        .await?;

    // A subagent deselects AGENTS.md and gets its own instructions, rendered last as the
    // `instructions` section.
    let subagent = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(obj(&format!(
                    r#"{{{model},"cwd":"/repo","extensions":{{"remove":[{{"name":"agents-md"}}]}},
                    "instructions":"Only read; never edit files."}}"#
                ))),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;

    root.submit(SubmissionDraft::input("Fix the build."), &context)
        .await?
        .wait(&context)
        .await?;
    subagent
        .submit(SubmissionDraft::input("Read the logs."), &context)
        .await?
        .wait(&context)
        .await?;
    let root_prompt = system_entries(&root, &context).await?;
    let sub_prompt = system_entries(&subagent, &context).await?;
    println!("root system prompt: {root_prompt:#?}");
    println!("subagent system prompt: {sub_prompt:#?}");
    assert_eq!(root_prompt.len(), 1);
    assert_eq!(
        root_prompt[0]["sections"],
        serde_json::json!({
            "preamble": "You are a coding agent. Be terse.",
            "cwd": "<cwd>\n/repo\n</cwd>",
            "agents_md": "<agents_md>\nRun npm run check after changes.\n</agents_md>",
        })
    );
    assert_eq!(sub_prompt.len(), 1);
    let sections = sub_prompt[0]["sections"].as_object().expect("sections");
    assert!(
        !sections.contains_key("agents_md"),
        "the subagent deselected AGENTS.md"
    );
    assert_eq!(sections["preamble"], "You are a coding agent. Be terse.");
    assert_eq!(sections["cwd"], "<cwd>\n/repo\n</cwd>");
    assert_eq!(
        sections["instructions"],
        "<instructions>\nOnly read; never edit files.\n</instructions>"
    );
    assert_eq!(
        sections.keys().next_back().map(String::as_str),
        Some("instructions")
    );

    // When a section's output changes, the next request appends only the change.
    root.configure(obj(r#"{"cwd":"/repo/packages"}"#), &context)
        .await?;
    root.submit(SubmissionDraft::input("Now the package."), &context)
        .await?
        .wait(&context)
        .await?;
    let after = system_entries(&root, &context).await?;
    println!("root system entries after cwd change: {after:#?}");
    assert_eq!(after.len(), 2);
    assert_eq!(
        after[1]["sections"],
        serde_json::json!({"cwd": "<cwd>\n/repo/packages\n</cwd>"})
    );

    harness.close(&context).await
}
