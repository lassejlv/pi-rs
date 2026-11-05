//! Scheduler fan-out benchmark. One model-issued tool makes `calls` parallel nested calls of a tool that returns at
//! once, on in-memory storage, so the time is the Harness's and the scheduler's own. Quadratic work shows as four
//! times the time per doubling of `calls`.
//!
//!   cargo bench -p pi-durable --bench scheduler_fanout          (PI_DURABLE_BENCH_QUICK=1 for small sizes)
mod support;
use pi_durable::harness::tool::NestedCallOptions;
use pi_durable::*;
use serde_json::json;
use std::{sync::Arc, time::Instant};
use support::*;

async fn run(calls: usize) -> Result<(u128, u64)> {
    let (models, faux) = faux(fast());
    let registry = registry();
    let leaf = tool(
        "leaf",
        "Return at once",
        r#"{"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}"#,
        |_, _, _| Box::pin(async { Ok(text_result("ok")) }),
    );
    let fan = tool(
        "fan",
        "Call leaf in parallel",
        r#"{"type":"object","properties":{"count":{"type":"number"}},"required":["count"]}"#,
        |args, api, context| {
            Box::pin(async move {
                let count = sj(args.value())["count"].as_u64().unwrap_or(0);
                let results = futures::future::join_all((0..count).map(|n| {
                    api.execute_tool(
                        "leaf",
                        object(&json!({"n": n}).to_string()),
                        &context,
                        NestedCallOptions {
                            progress: Some(false),
                            ..Default::default()
                        },
                    )
                }))
                .await;
                let failed = results
                    .iter()
                    .filter(|result| !matches!(result, Ok(result) if !result.is_error))
                    .count();
                let mut result = text_result(&format!("{failed} failed"));
                result.is_error = Some(failed > 0);
                Ok(result)
            })
        },
    );
    let mut extension = Ext::new("bench");
    extension.tools.push(Arc::new(leaf));
    extension.tools.push(Arc::new(fan));
    install(&registry, extension);
    let (storage, tally) = Counted::wrap(Arc::new(MemoryStorage::new()));
    let context = ctx();
    let harness = Harness::open(storage, HarnessOptions::new(registry, models), &context).await?;
    let root = harness.root(root_options(), &context).await?;
    faux.set_responses(vec![
        call("fan", json!({"count": calls}), "fan"),
        answer("done"),
    ]);
    let started = Instant::now();
    let settled = root
        .submit(SubmissionDraft::input("go"), &context)
        .await?
        .wait(&context)
        .await?;
    if settled.as_record().state.status() != SubmissionStatus::Done {
        return Err(bench_err("run did not finish"));
    }
    harness.wait_for_idle(&context).await?;
    let ms = started.elapsed().as_millis();
    // A run whose nested calls failed measures nothing.
    assert_tool_succeeded(&root, "fan-out").await?;
    harness.close(&context).await?;
    Ok((ms, tally.snapshot().0))
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("calls | ms     | commits | ms per call");
    for calls in sizes(&[250, 500, 1000, 2000, 4000], &[50, 100]) {
        let (ms, commits) = run(calls).await?;
        println!(
            "{:<5} | {:<6} | {:<7} | {:.2}",
            calls,
            ms,
            commits,
            ms as f64 / calls as f64
        );
    }
    Ok(())
}
