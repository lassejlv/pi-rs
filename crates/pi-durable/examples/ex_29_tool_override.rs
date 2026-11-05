//! Replace a tool for some conversations, and decorate whichever tool wins: a bash that runs inside a Python
//! virtualenv, and a wrapper that times every bash call.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::tools::{BashToolOptions, coding_tools, create_bash_tool};
use pi_durable::*;
use serde_json::json;
use std::{
    sync::{Arc, LazyLock},
    time::Instant,
};
use support::*;

// ─── Product code ───────────────────────────────────────────────────────────

// A bash with the same name: where selected after the coding tools, it replaces their bash in place.
fn venv_extension() -> Ext {
    let mut extension = Ext::new("venv");
    extension
        .tools
        .push(Arc::new(create_bash_tool(BashToolOptions {
            command_prefix: Some("source .venv/bin/activate".into()),
            ..Default::default()
        })));
    extension
}

// Wraps the bash that won, whichever it is. Wrappers never capture a base tool, so reloading either bash keeps it.
static TIMINGS: LazyLock<Mutex<Vec<u128>>> = LazyLock::new(Mutex::default);
fn timing_extension() -> Ext {
    let mut extension = Ext::new("timing");
    extension.wraps.push(wrap_tool(
        &create_bash_tool(Default::default()),
        |bash: Arc<NativeTool>| {
            let inner = bash.clone();
            Ok(Arc::new(NativeTool {
                declaration: bash.declaration.clone(),
                callers: bash.callers.clone(),
                deferred: bash.deferred,
                replay: bash.replay,
                execution_mode: bash.execution_mode,
                output_limits: bash.output_limits,
                structured_output_schema: bash.structured_output_schema.clone(),
                metadata: bash.metadata.clone(),
                prepare_arguments: bash.prepare_arguments.clone(),
                validate_arguments: bash.validate_arguments.clone(),
                execute: Arc::new(move |args, api, context| {
                    let inner = inner.clone();
                    Box::pin(async move {
                        let start = Instant::now();
                        let result = (inner.execute)(args, api, context).await;
                        TIMINGS.lock().push(start.elapsed().as_millis());
                        result
                    })
                }),
            }))
        },
    ));
    extension
}

// ─── Host setup ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let project = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    std::fs::create_dir_all(project.path().join(".venv/bin"))
        .map_err(|e| Error::message(e.to_string()))?;
    std::fs::write(
        project.path().join(".venv/bin/activate"),
        format!(
            "export VIRTUAL_ENV=\"{}/.venv\"\n",
            project.path().display()
        ),
    )
    .map_err(|e| Error::message(e.to_string()))?;

    let (models, faux) = faux(fast());
    let probe = || call("bash", json!({"command": "echo venv: $VIRTUAL_ENV"}), None);
    faux.set_responses(vec![probe(), answer("Done."), probe(), answer("Done.")]);

    let registry = registry();
    install(&registry, coding_tools());
    install(&registry, timing_extension());
    install(&registry, venv_extension());
    // Venv is installed but not selected by default.
    let mut options = HarnessOptions::new(registry, models).with_settings(HarnessSettings {
        extensions: Some(vec!["coding-tools".into(), "timing".into()]),
        ..Default::default()
    });
    options.env = Some(cwd_env(project.path().to_path_buf()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let plain = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;
    let python = harness
        .create_conversation(
            ConversationCreateOptions {
                agent: Some(agent(
                    "faux-1",
                    json!({"extensions": {"add": [{"name": "venv"}]}}),
                )),
                ..ConversationCreateOptions::new(ConversationOwnership::Ownerless)
            },
            &context,
        )
        .await?;

    let probe_bash = |conversation: Conversation| async move {
        conversation
            .submit(SubmissionDraft::input("Which venv?"), &ctx())
            .await?
            .wait(&ctx())
            .await?;
        last_tool_result(&conversation).await
    };
    let plain_out = probe_bash(plain.clone()).await?;
    println!("plain conversation: {plain_out}");
    let python_out = probe_bash(python.clone())
        .await?
        .replace(&project.path().display().to_string(), "<project>");
    println!("python conversation: {python_out}");
    println!("timed bash calls: {}", TIMINGS.lock().len());
    check(
        plain_out == "venv:",
        "the plain conversation has no virtualenv",
    )?;
    check(
        python_out == "venv: <project>/.venv",
        "the python conversation's bash ran inside the virtualenv",
    )?;
    check(
        TIMINGS.lock().len() == 2,
        "the wrapper timed both bash calls, whichever bash won",
    )?;
    harness.close(&context).await
}
