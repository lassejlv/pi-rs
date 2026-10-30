//! Print mode: submit one prompt and print its answer, like `pi -p`. The host awaits its own Submission, not
//! global idle. Uses a scripted faux model (this port is offline).
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_durable::*;
use serde_json::json;
use std::sync::Arc;
use support::*;

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "What is in this directory?".into());
    let project = sample_project()?;
    let (models, faux) = faux(fast());
    faux.set_responses(vec![
        call("bash", json!({"command": "ls"}), Some("call-1")),
        answer("This directory holds the durable package sources, tests, and docs."),
    ]);
    let registry = registry();
    install(&registry, coding_extension("coding"));
    let mut options = HarnessOptions::new(registry, models);
    options.env = Some(cwd_env(project.path().to_path_buf()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;

    let submission = root
        .submit(SubmissionDraft::input(prompt.as_str()), &context)
        .await?;
    let settled = submission.wait(&context).await?;
    if status_of(&settled) != SubmissionStatus::Done {
        return Err(Error::message(format!(
            "unanswered: {:?}",
            settled.as_record().state
        )));
    }
    let text = answer_text(&root, &settled).await?;
    println!("{text}");
    check(
        text == "This directory holds the durable package sources, tests, and docs.",
        "printed answer",
    )?;
    // The scripted `ls` actually ran in the project directory.
    let listing = last_tool_result(&root).await?;
    check(
        listing.contains("README.md") && listing.contains("package.json"),
        "ls output",
    )?;
    harness.close(&context).await
}
