//! A coding-agent tool turn on JSONL storage: the model reads, edits, and runs commands, then answers. A hook
//! times a `cat` of a file that does not exist, which ends in an error result.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::tools::coding_tools;
use pi_durable::*;
use serde_json::json;
use std::{sync::Arc, time::Instant};
use support::*;

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let directory = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    let notes = directory.path().join("notes.txt");
    std::fs::write(&notes, "hello world\n").map_err(|e| Error::message(e.to_string()))?;
    let missing = directory.path().join("1gb.txt");

    // The faux provider plays the model: four tool-calling answers, then a final answer.
    let (models, faux) = faux(fast());
    faux.set_responses(vec![
        call("read", json!({"path": "notes.txt"}), Some("r")),
        call(
            "edit",
            json!({"path": "notes.txt", "edits": [{"oldText": "world", "newText": "durable"}]}),
            Some("e"),
        ),
        call("bash", json!({"command": "cat notes.txt"}), Some("b")),
        call(
            "bash",
            json!({"command": format!("cat {}", missing.display())}),
            Some("c"),
        ),
        answer("The file now greets durable."),
    ]);

    // Hooks see every call before and after execution; these time the big `cat`.
    let is_big_cat = |call: &ToolHookCall| {
        call.call.name.equals_str("bash")
            && sj(&call.call.arguments)["command"]
                .as_str()
                .is_some_and(|command| command.contains("1gb.txt"))
    };
    let start = Arc::new(Mutex::new(None::<Instant>));
    let timed = Arc::new(Mutex::new(None::<u128>));
    let (begin, finish, timed_out) = (start.clone(), start.clone(), timed.clone());
    let timing = ToolHooks {
        before_tool: Some(Arc::new(move |call, _, _| {
            if is_big_cat(&call) {
                *begin.lock() = Some(Instant::now());
            }
            Box::pin(async { Ok(None) })
        })),
        after_tool: Some(Arc::new(move |call, _, _, _| {
            if is_big_cat(&call)
                && let Some(started) = finish.lock().take()
            {
                *timed_out.lock() = Some(started.elapsed().as_millis());
                println!("cat 1gb.txt took {} ms", started.elapsed().as_millis());
            }
            Box::pin(async { Ok(None) })
        })),
        provider: None,
    };
    let registry = registry();
    // CodingTools brings read, write, edit, and bash. Tools reach files and processes only through the
    // environment the Harness builds for each call.
    install(&registry, coding_tools());
    let mut extension = Ext::new("timing");
    extension.hooks.push(hook(&*TOOL_TASK, Arc::new(timing)));
    install(&registry, extension);

    // The environment is built per use and follows the conversation's agent `cwd`.
    let storage =
        JsonlStorage::open_native(directory.path().join("store"), &context, Default::default())
            .await?;
    let mut options = HarnessOptions::new(registry, models);
    options.env = Some(cwd_env(directory.path().to_path_buf()));
    let harness = Harness::open(Arc::new(storage), options, &context).await?;
    let root = harness
        .root(
            root_options("faux-1", json!({"cwd": directory.path().to_string_lossy()})),
            &context,
        )
        .await?;

    // Each tool call runs as a durable pi.tool task owned by the generation.
    let settled = root
        .submit(SubmissionDraft::input("Greet durable instead."), &context)
        .await?
        .wait(&context)
        .await?;
    println!("status: {:?}", status_of(&settled));
    check(
        status_of(&settled) == SubmissionStatus::Done,
        "submission done",
    )?;

    let mut results = Vec::new();
    for entry in transcript(&root, 20).await? {
        if !is_kind(&entry, "pi.tool-result") {
            println!("{}", entry.kind);
            continue;
        }
        let result = message(&entry).expect("tool result message");
        let text = content_text(&result["content"]);
        let shown: String = text.chars().take(200).collect();
        println!(
            "{} {}: {:?}",
            entry.kind,
            result["toolName"].as_str().unwrap_or(""),
            shown
        );
        results.push((
            result["toolName"].as_str().unwrap_or("").to_string(),
            result["isError"].as_bool().unwrap_or(false),
            text,
        ));
    }
    check(results.len() == 4, "four tool results")?;
    check(
        results.iter().map(|r| r.0.as_str()).collect::<Vec<_>>()
            == ["read", "edit", "bash", "bash"],
        "tool order",
    )?;
    check(
        results[0].2.contains("hello world"),
        "read saw the original text",
    )?;
    check(
        results[2].2.contains("hello durable"),
        "bash cat saw the edit",
    )?;
    check(results[3].1, "cat of a missing file is an error result")?;

    let answer = answer_text(&root, &settled).await?;
    println!("answer: {answer:?}");
    check(answer == "The file now greets durable.", "final answer")?;
    let file = std::fs::read_to_string(&notes).map_err(|e| Error::message(e.to_string()))?;
    println!("file: {file:?}");
    check(file == "hello durable\n", "file edited")?;
    check(timed.lock().is_some(), "hooks timed the big cat")?;
    harness.close(&context).await?;
    println!("storage: {}", directory.path().join("store").display());
    Ok(())
}
