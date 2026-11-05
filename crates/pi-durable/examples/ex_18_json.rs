//! JSON mode: stream one prompt's run as JSON lines, like `pi --mode json`. Two modes:
//!   events: the agent events of `watch_events`, starting with a `snapshot` event.
//!   ops: the raw conversation view frames; each later frame holds one commit's ops.
//! Each mode runs against SQLite, JSONL and memory storage. Uses a scripted faux model.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;

async fn run(mode: &str, kind: &str) -> Result<Vec<Value>> {
    let context = ctx();
    let project = sample_project()?;
    let location = tempfile::tempdir().map_err(|e| Error::message(e.to_string()))?;
    let storage: Arc<dyn Storage> = match kind {
        "sqlite" => Arc::new(
            open_native_sqlite_storage(
                location.path().join("json.sqlite"),
                NativeSqliteStorageOptions::default(),
            )
            .await?,
        ),
        "jsonl" => Arc::new(
            JsonlStorage::open_native(location.path(), &context, Default::default()).await?,
        ),
        _ => Arc::new(MemoryStorage::new()),
    };
    let (models, faux) = faux(RegisterOptions {
        tokens_per_second: Some(200.0),
        ..Default::default()
    });
    faux.set_responses(vec![
        call("bash", json!({"command": "ls"}), Some("call-1")),
        answer("This directory holds the durable package sources, tests, and docs."),
    ]);
    let registry = registry();
    install(&registry, coding_extension("coding"));
    let mut options = HarnessOptions::new(registry, models);
    options.env = Some(cwd_env(project.path().to_path_buf()));
    let harness = Harness::open(storage, options, &context).await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;

    let lines = Arc::new(Mutex::new(Vec::<Value>::new()));
    let print = {
        let lines = lines.clone();
        move |value: Value| lines.lock().push(value)
    };
    // Attach before submitting, so the stream covers the whole run.
    let stop: Box<dyn Fn()>;
    if mode == "events" {
        let stream = harness.watch_events(root.id, &context).await?;
        print(
            serde_json::to_value(stream.snapshot.as_ref())
                .map_err(|e| Error::message(e.to_string()))?,
        );
        let sink = print.clone();
        stream.start(move |events, _| {
            let sink = sink.clone();
            async move {
                for event in events.iter() {
                    sink(serde_json::to_value(event).map_err(|e| e.to_string())?);
                }
                Ok(())
            }
        })?;
        stop = Box::new(move || {
            stream.stop();
        });
    } else {
        let watch = root.watch(&context).await?;
        print(json!({"view": {"entries": watch.value().entries.len()}}));
        let sink = print.clone();
        watch
            .start(move |frame| {
                let sink = sink.clone();
                async move {
                    sink(json!({"ops": frame.ops.len(), "entries": frame.value.entries.len()}));
                    Ok(())
                }
            })
            .map_err(Error::message)?;
        stop = Box::new(move || {
            watch.stop();
        });
    }
    let submission = root
        .submit(
            SubmissionDraft::input("What is in this directory?"),
            &context,
        )
        .await?;
    submission.wait(&context).await?;
    harness.wait_for_idle(&context).await?;
    // Let the last batch reach the listener before stopping.
    pause(50).await;
    stop();
    harness.close(&context).await?;
    let lines = lines.lock().clone();
    Ok(lines)
}

type RegisterOptions = pi_ai::providers::faux::RegisterFauxProviderOptions;

#[tokio::main]
async fn main() -> Result<()> {
    for kind in ["sqlite", "jsonl", "memory"] {
        let events = run("events", kind).await?;
        let types: Vec<&str> = events
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        println!("{kind} events: {} lines, types {:?}", events.len(), types);
        check(
            types.first() == Some(&"snapshot"),
            "events start with a snapshot",
        )?;
        for expected in [
            "run_start",
            "tool_execution_start",
            "tool_execution_end",
            "message_end",
            "run_end",
        ] {
            check(types.contains(&expected), expected)?;
        }
        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_execution_end")
            .unwrap();
        check(ended["toolName"] == "bash", "bash tool ended")?;
        let last_text = events
            .iter()
            .filter(|event| {
                event["type"] == "message_end" && event["entry"]["kind"] == "pi.assistant"
            })
            .filter_map(|event| event["entry"]["model"][0]["content"][0]["text"].as_str())
            .next_back();
        check(
            last_text == Some("This directory holds the durable package sources, tests, and docs."),
            "final assistant message",
        )?;

        let ops = run("ops", kind).await?;
        println!("{kind} ops: {} lines", ops.len());
        check(ops[0].get("view").is_some(), "ops start with the view")?;
        check(
            ops.len() > 1
                && ops[1..]
                    .iter()
                    .all(|line| line["ops"].as_u64().unwrap_or(0) > 0),
            "later lines carry ops",
        )?;
        check(
            ops.last().unwrap()["entries"].as_u64().unwrap_or(0) >= 4,
            "view grew to the full transcript",
        )?;
    }
    Ok(())
}
