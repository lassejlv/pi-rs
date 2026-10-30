//! A small coding agent: the built-in coding tools, a prompt that knows the working directory, settings backed by
//! the app's settings file, and an environment that follows the conversation's directory.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::tools::coding_tools;
use pi_durable::*;
use serde_json::json;
use std::sync::Arc;
use support::*;

struct UserSettings {
    parallel_tools: bool,
    max_retries: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let workspace = tempfile::Builder::new()
        .prefix("pi-durable-agent-")
        .tempdir()
        .map_err(|e| Error::message(e.to_string()))?;
    let app = workspace.path().join("app");
    std::fs::create_dir(&app).map_err(|e| Error::message(e.to_string()))?;
    let canonical = |path: &std::path::Path| std::fs::canonicalize(path).unwrap();

    // The app's own prompt, next to the coding tools.
    let mut coding = Ext::new("coding");
    coding.sections.push(Arc::new(section(
        "preamble",
        Arc::new(|_, _| {
            Box::pin(async {
                Ok(Some(
                    "You are a coding agent. Use the tools to inspect the project.".into(),
                ))
            })
        }),
        SectionOptions { tag: Some(false) },
    )));
    coding.sections.push(Arc::new(section(
        "cwd",
        Arc::new(|input, _| {
            let cwd = input
                .env
                .as_ref()
                .map(|env| env.cwd().to_string_lossy().into_owned());
            Box::pin(async move { Ok(cwd.map(Into::into)) })
        }),
        SectionOptions::default(),
    )));
    let registry = registry();
    install(&registry, coding_tools());
    install(&registry, coding);

    // Settings the user edits while the agent runs, like a settings.json the app watches. The reader makes every
    // Harness read see the current values; nothing is copied or stored.
    let user = Arc::new(Mutex::new(UserSettings {
        parallel_tools: true,
        max_retries: 3,
    }));
    let reads = Arc::new(Mutex::new(Vec::<String>::new()));
    let (current, seen) = (user.clone(), reads.clone());
    let (models, faux) = faux(fast());
    let pwd = || call("bash", json!({"command": "pwd"}), None);
    faux.set_responses(vec![pwd(), answer("Done."), pwd(), answer("Done.")]);
    let mut options = HarnessOptions::new(registry, models);
    options.settings = Arc::new(move || {
        let user = current.lock();
        let mode = if user.parallel_tools {
            "parallel"
        } else {
            "sequential"
        };
        seen.lock().push(mode.to_string());
        Ok(HarnessSettings {
            tool_execution: Some(mode.into()),
            retry: Some(object(&format!(r#"{{"maxRetries":{}}}"#, user.max_retries))),
            ..Default::default()
        })
    });
    // The Harness calls this for every tool call and request with the conversation's `cwd`, so changing a
    // conversation's directory needs no restart.
    options.env = Some(cwd_env(workspace.path().to_path_buf()));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context).await?;
    let root = harness
        .root(
            root_options("faux-1", json!({"cwd": workspace.path().to_string_lossy()})),
            &context,
        )
        .await?;

    root.submit(SubmissionDraft::input("Where are we?"), &context)
        .await?
        .wait(&context)
        .await?;
    let first = last_tool_result(&root).await?;
    println!("bash ran in: {first}");
    check(
        canonical(std::path::Path::new(&first)) == canonical(workspace.path()),
        "bash ran in the workspace",
    )?;

    // The user switches the project directory and turns off parallel tools. Both apply from the next use.
    root.configure(
        object(&json!({"cwd": app.to_string_lossy()}).to_string()),
        &context,
    )
    .await?;
    user.lock().parallel_tools = false;
    root.submit(SubmissionDraft::input("And now?"), &context)
        .await?
        .wait(&context)
        .await?;
    let second = last_tool_result(&root).await?;
    println!("bash ran in: {second}");
    check(
        canonical(std::path::Path::new(&second)) == canonical(&app),
        "bash followed the new directory",
    )?;

    let modes = reads.lock().clone();
    check(
        modes.iter().any(|m| m == "parallel") && modes.iter().any(|m| m == "sequential"),
        "settings were read live",
    )?;
    // The prompt knew the working directory each time.
    let view = root.context(None, &context).await?;
    let systems: Vec<String> = view
        .messages
        .iter()
        .map(|m| sj(m))
        .filter(|m| m["role"] == "system")
        .map(|m| serde_json::to_string(&m).unwrap())
        .collect();
    check(
        systems.iter().any(|s| s.contains("You are a coding agent")),
        "the preamble is in the prompt",
    )?;
    check(
        systems.iter().any(|s| s.contains("app")),
        "the changed directory reached the prompt",
    )?;
    harness.close(&context).await
}
