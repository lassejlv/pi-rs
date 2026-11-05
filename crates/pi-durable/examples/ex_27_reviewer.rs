//! A reviewer agent next to the main one: a cheaper model, a review role and loop, read-only tools, and its own
//! checkout of the project.
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_ai::providers::faux::{FauxModelDefinition, RegisterFauxProviderOptions};
use pi_durable::harness::registry::HookRegistration;
use pi_durable::tools::coding_tools;
use pi_durable::*;
use serde_json::json;
use std::sync::Arc;
use support::*;

const DONE: &str = "No further findings.";

// ─── Product code: the reviewer extension ───────────────────────────────────

// A role and a review loop: every answer that still has findings gets a second pass.
fn reviewer_extension() -> Ext {
    let mut extension = Ext::new("reviewer");
    extension.sections.push(Arc::new(static_section(
        "role",
        "You review diffs. Report problems as a list. Never edit files.",
    )));
    let hooks = GenerationHooks {
        on_yield: Some(Arc::new(|message, _, _| {
            Box::pin(async move {
                let text = message
                    .text_blocks()?
                    .iter()
                    .map(|block| block.to_string())
                    .collect::<Vec<_>>()
                    .join("");
                Ok(if text.contains(DONE) {
                    None
                } else {
                    Some(
                        format!(r#"Look again for anything you missed. Say "{DONE}" when there is nothing left."#)
                            .as_str()
                            .into(),
                    )
                })
            })
        })),
        ..Default::default()
    };
    extension
        .hooks
        .push(HookRegistration::new("pi.generation", Arc::new(hooks)));
    extension
}

// ─── Host setup ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    // The reviewer works in its own checkout, in practice a `git worktree add`.
    let worktree = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    std::fs::write(
        worktree.path().join("user.ts"),
        "export const name = (user) => user.name;\n",
    )
    .map_err(|e| Error::message(e.to_string()))?;

    let (models, faux) = faux(RegisterFauxProviderOptions {
        models: vec![
            FauxModelDefinition {
                id: "big".into(),
                ..Default::default()
            },
            FauxModelDefinition {
                id: "small".into(),
                ..Default::default()
            },
        ],
        tokens_per_second: Some(f64::INFINITY),
        ..Default::default()
    });
    faux.set_responses(vec![
        call("read", json!({"path": "user.ts"}), None),
        answer("1. `name` does not handle a missing user."),
        answer(&format!("2. `user` has no type. {DONE}")),
    ]);

    let registry = registry();
    install(&registry, coding_tools());
    install(&registry, reviewer_extension());
    // The main agent selects only the coding tools; the reviewer opts in.
    let mut options = HarnessOptions::new(registry, models).with_settings(HarnessSettings {
        extensions: Some(vec!["coding-tools".into()]),
        ..Default::default()
    });
    options.env = Some(cwd_env(
        std::env::current_dir().map_err(|e| Error::message(e.to_string()))?,
    ));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    harness
        .root(root_options("big", json!({})), &context)
        .await?;

    // Everything the reviewer is, stored on its conversation: the model, exactly these extensions in this order,
    // only the read tool, and its directory. A restart keeps all of it.
    let reviewer = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(agent(
                    "small",
                    json!({
                        "extensions": [{"name": "coding-tools"}, {"name": "reviewer"}],
                        "tools": [{"name": "read"}],
                        "cwd": worktree.path().to_string_lossy(),
                    }),
                )),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;
    let resolved = agent_of(&reviewer).await?;
    let model = sj(resolved.model.as_ref().expect("reviewer model"));
    println!(
        "reviewer: {} {:?} {:?} {}",
        model["modelId"],
        extension_names(&resolved),
        tool_names(&resolved),
        resolved
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.to_string() == worktree.path().to_string_lossy())
    );
    check(
        model["modelId"] == "small",
        "the reviewer uses the cheaper model",
    )?;
    check(
        extension_names(&resolved) == ["coding-tools", "reviewer"],
        "exactly these extensions in this order",
    )?;
    check(tool_names(&resolved) == ["read"], "read-only tools")?;
    check(
        resolved
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.to_string() == worktree.path().to_string_lossy()),
        "its own directory",
    )?;

    reviewer
        .submit(SubmissionDraft::input("Review user.ts."), &context)
        .await?
        .wait(&context)
        .await?;
    let mut findings = Vec::new();
    for entry in transcript(&reviewer, 20).await? {
        let text = message_text(&entry);
        if is_kind(&entry, "pi.user") {
            println!("> {text}");
        }
        if is_kind(&entry, "pi.assistant") && !text.is_empty() {
            println!("reviewer: {text}");
            findings.push(text);
        }
    }
    check(
        findings
            == [
                "1. `name` does not handle a missing user.".to_string(),
                format!("2. `user` has no type. {DONE}"),
            ],
        "the review loop gave a second pass until done",
    )?;
    let reads = transcript(&reviewer, 20)
        .await?
        .iter()
        .filter(|entry| is_kind(entry, "pi.tool-result"))
        .map(message_text)
        .collect::<Vec<_>>();
    check(
        reads.len() == 1 && reads[0].contains("export const name"),
        "the reviewer read its own checkout",
    )?;
    harness.close(&context).await
}
