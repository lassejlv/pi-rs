//! Reload an extension while a call runs, then restart the process. Running work finishes on the code it started
//! with; stored choices are names, so they survive a restart and bind to whatever code the new process installs.
#[path = "ex_b_support/mod.rs"]
mod support;
use pi_ai::models::Models;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde_json::json;
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};
use support::*;

// Stand-in for code loaded from disk: each call builds the extension as the file currently reads.
static HOLD: AtomicBool = AtomicBool::new(false);
static STARTED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
static GATE: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
fn load_versioned(version: &'static str) -> Ext {
    let mut extension = Ext::new("versioned");
    extension.tools.push(Arc::new(tool(
        "version",
        "Report the tool's code version",
        r#"{"type":"object","properties":{}}"#,
        move |_, _, _| {
            Box::pin(async move {
                if HOLD.swap(false, Ordering::SeqCst) {
                    STARTED.notify_one();
                    GATE.notified().await;
                }
                Ok(text_result(version))
            })
        },
    )));
    extension
}

async fn open(registry: &Reg, path: &std::path::Path, models: Models) -> Result<Harness> {
    let storage = open_native_sqlite_storage(path, NativeSqliteStorageOptions::default()).await?;
    Harness::open(
        Arc::new(storage),
        HarnessOptions::new(registry.clone(), models),
        &ctx(),
    )
    .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let (models, faux) = faux(fast());
    let call_version = || call("version", json!({}), None);
    faux.set_responses(vec![
        call_version(),
        answer("Done."),
        call_version(),
        answer("Done."),
    ]);
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    let path = directory.path().join("session.sqlite");

    // First process.
    let registry_one = registry();
    install(&registry_one, load_versioned("v1"));
    let harness = open(&registry_one, &path, models.clone()).await?;
    // Selected by name. The name is what is stored, never the code.
    let root = harness
        .root(
            root_options("faux-1", json!({"extensions": [{"name": "versioned"}]})),
            &context,
        )
        .await?;

    // The file changes while a call runs: the running call finishes on v1, the next call uses v2.
    HOLD.store(true, Ordering::SeqCst);
    let submission = root
        .submit(SubmissionDraft::input("Which version?"), &context)
        .await?;
    STARTED.notified().await;
    install(&registry_one, load_versioned("v2"));
    GATE.notify_one();
    submission.wait(&context).await?;
    let during = last_tool_result(&root).await?;
    println!("call running during the reload: {during}");
    check(
        during == "v1",
        "the running call finished on the code it started with",
    )?;
    root.submit(SubmissionDraft::input("And now?"), &context)
        .await?
        .wait(&context)
        .await?;
    let next = last_tool_result(&root).await?;
    println!("next call: {next}");
    check(next == "v2", "the next call uses the reloaded code")?;
    harness.close(&context).await?;

    // Second process: the conversation still selects "versioned", but this process has not installed it yet.
    let registry_two = registry();
    let harness = open(&registry_two, &path, models).await?;
    let root = harness.root(Default::default(), &context).await?;
    let before = tool_names(&agent_of(&root).await?);
    println!("after restart, before install: {before:?}");
    check(
        before.is_empty(),
        "no tool before the extension is installed",
    )?;
    install(&registry_two, load_versioned("v3"));
    let after = tool_names(&agent_of(&root).await?);
    println!("after install: {after:?}");
    check(
        after == ["version"],
        "the stored name binds to the newly installed code",
    )?;
    harness.close(&context).await
}
