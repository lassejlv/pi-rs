//! Late join: a client attaches while a run is already underway. It gets the current state first, the
//! conversation view or the snapshot event, and then only what changes after that.
#[path = "ex_b_support/mod.rs"]
mod support;
use parking_lot::Mutex;
use pi_ai::providers::faux::RegisterFauxProviderOptions;
use pi_durable::*;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    // A tool that prints a line every 100 ms, then a slowly streamed answer.
    let registry = registry();
    let mut count = Ext::new("count");
    count.tools.push(Arc::new(tool(
        "count",
        "Counts to ten",
        r#"{"type":"object","properties":{}}"#,
        |_, api, _| {
            Box::pin(async move {
                for n in 1..=10 {
                    api.output(format!("{n}\n"), None)?;
                    pause(100).await;
                }
                Ok(ToolExecutionResult::default())
            })
        },
    )));
    install(&registry, count);
    let (models, faux) = faux(RegisterFauxProviderOptions {
        tokens_per_second: Some(40.0),
        token_size_min: Some(1),
        token_size_max: Some(1),
        ..Default::default()
    });
    faux.set_responses(vec![
        call("count", json!({}), Some("call-1")),
        answer("Counted to ten, and this answer streams slowly."),
    ]);
    let harness = Harness::open(
        Arc::new(MemoryStorage::new()),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let root = harness
        .root(root_options("faux-1", json!({})), &context)
        .await?;

    let submission = root
        .submit(
            SubmissionDraft::input("Count to ten, then tell me."),
            &context,
        )
        .await?;
    // Join while the tool is halfway through.
    pause(500).await;

    // Structural client: the view holds the committed transcript and documents, including the running tool's
    // output.
    let view = root.watch(&context).await?;
    let live = live_of(&view.value());
    let entries: Vec<String> = view
        .value()
        .entries
        .iter()
        .map(|e| e.kind.to_string())
        .collect();
    println!("view entries: {entries:?}");
    let slot = live["tools"][0].clone();
    println!("view tool slot: {} {}", slot["status"], slot["output"]);
    check(
        slot["status"] == "running",
        "the view shows the tool running",
    )?;
    check(
        slot["output"]
            .as_str()
            .is_some_and(|o| o.starts_with("1\n")),
        "the view carries output so far",
    )?;
    let view_outputs = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = view_outputs.clone();
    view.start(move |frame| {
        let seen = seen.clone();
        async move {
            let live = live_of(&frame.value);
            if live["tools"][0]["status"] == "running"
                && let Some(output) = live["tools"][0]["output"].as_str()
            {
                println!("view output now: {output:?}");
                seen.lock().push(output.to_string());
            }
            Ok(())
        }
    })
    .map_err(Error::message)?;

    // Event client: the snapshot event carries the same state; later events apply on top of it.
    let stream = harness.watch_events(root.id, &context).await?;
    let snapshot = serde_json::to_value(stream.snapshot.as_ref())
        .map_err(|e| Error::message(e.to_string()))?;
    let tools: Vec<String> = snapshot["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|slot| {
            format!(
                "{} {}",
                slot["name"].as_str().unwrap(),
                slot["status"].as_str().unwrap()
            )
        })
        .collect();
    println!("snapshot tools: {tools:?}");
    check(
        tools == ["count running"],
        "snapshot shows the running tool",
    )?;
    let output = Arc::new(Mutex::new(
        snapshot["tools"][0]["output"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    ));
    let event_types = Arc::new(Mutex::new(Vec::<String>::new()));
    let (state, types) = (output.clone(), event_types.clone());
    stream.start(move |events, _| {
        let (state, types) = (state.clone(), types.clone());
        async move {
            for event in events.iter() {
                let event: Value = serde_json::to_value(event).map_err(|e| e.to_string())?;
                let kind = event["type"].as_str().unwrap_or("").to_string();
                let change = &event["output"];
                if kind == "tool_execution_update" && !change["set"].is_null() {
                    *state.lock() = change["set"].as_str().unwrap_or("").to_string();
                    println!("event output now: {:?}", state.lock());
                } else if kind == "tool_execution_update" && !change["append"].is_null() {
                    let mut state = state.lock();
                    let trim = change["trimStart"].as_u64().unwrap_or(0) as usize;
                    let kept: String = state.chars().skip(trim).collect();
                    *state = kept + change["append"].as_str().unwrap_or("");
                    println!("event output now: {:?}", *state);
                } else if kind == "message_update" {
                    let deltas: String = event["changes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|change| change["type"] == "text_delta")
                        .filter_map(|change| change["delta"].as_str())
                        .collect();
                    println!("event text delta: {deltas:?}");
                } else {
                    println!("event: {kind}");
                }
                types.lock().push(kind);
            }
            Ok(())
        }
    })?;

    submission.wait(&context).await?;
    harness.wait_for_idle(&context).await?;
    pause(50).await;
    stream.stop();
    view.stop();
    let types = event_types.lock().clone();
    check(
        types.iter().any(|kind| kind == "tool_execution_end"),
        "events saw the tool end",
    )?;
    check(
        types.iter().any(|kind| kind == "message_update"),
        "events streamed the answer",
    )?;
    check(
        types.iter().any(|kind| kind == "run_end"),
        "events saw the run end",
    )?;
    check(
        output.lock().starts_with("1\n"),
        "the tool output stayed consistent",
    )?;
    check(
        !view_outputs.lock().is_empty(),
        "the view client saw live output updates",
    )?;
    harness.close(&context).await
}
