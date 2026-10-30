//! Durable code mode overhead. One model-issued call makes N calls of a trivial `echo` tool, either sequentially or
//! all at once, in two ways: a code mode script (`tools.echo()`, journaled and replayable) and a plain tool calling
//! `api.execute_tool()` the same way (nested calls, no journal). Reports wall time and commits per call on SQLite.
//!
//!   cargo bench -p pi-durable --bench codemode          (PI_DURABLE_BENCH_QUICK=1 for small N)
mod support;
use pi_durable::codemode::{CodemodeToolOptions, create_codemode_extension, vm};
use pi_durable::harness::tool::{NestedCallOptions, ReplayPolicy};
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde_json::json;
use std::{sync::Arc, time::Instant};
use support::*;

async fn run(codemode: bool, sequential: bool, n: usize) -> Result<(f64, u64)> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let (models, faux) = faux(fast());
    let echo = tool(
        "echo",
        "echo",
        r#"{"type":"object","properties":{"i":{"type":"number"}},"required":["i"]}"#,
        |args, _, _| Box::pin(async move { Ok(text_result(&sj(args.value())["i"].to_string())) }),
    );
    let mut plain = tool(
        "plain",
        "plain",
        r#"{"type":"object","properties":{"n":{"type":"number"},"shape":{"type":"string"}},"required":["n","shape"]}"#,
        |args, api, context| {
            Box::pin(async move {
                let args = sj(args.value());
                let n = args["n"].as_u64().unwrap_or(0);
                let call = |i: u64| {
                    api.execute_tool(
                        "echo",
                        object(&json!({"i": i}).to_string()),
                        &context,
                        NestedCallOptions::default(),
                    )
                };
                if args["shape"] == "sequential" {
                    for i in 0..n {
                        call(i).await?;
                    }
                } else {
                    for result in futures::future::join_all((0..n).map(call)).await {
                        result?;
                    }
                }
                Ok(text_result("done"))
            })
        },
    );
    plain.replay = ReplayPolicy::Safe;
    let sandbox = Arc::new(vm::NativeSandbox::new().map_err(bench_err)?);
    let mut options = CodemodeToolOptions::new(sandbox);
    options.models = false;
    let registry = registry();
    let mut bench = Ext::new("bench");
    bench.tools.push(Arc::new(echo));
    bench.tools.push(Arc::new(plain));
    install(&registry, bench);
    install(&registry, create_codemode_extension(options));
    let code = if sequential {
        format!(r#"for (let i = 0; i < {n}; i++) await tools.echo({{ i }}); return "done";"#)
    } else {
        format!(
            r#"await Promise.all(Array.from({{ length: {n} }}, (_, i) => tools.echo({{ i }}))); return "done";"#
        )
    };
    let step = if codemode {
        call("codemode", json!({"code": code}), "c1")
    } else {
        call(
            "plain",
            json!({"n": n, "shape": if sequential { "sequential" } else { "parallel" }}),
            "c1",
        )
    };
    faux.set_responses(vec![step, answer("ok")]);
    let storage = open_native_sqlite_storage(
        directory.path().join("session.sqlite"),
        NativeSqliteStorageOptions::default(),
    )
    .await?;
    let (storage, tally) = Counted::wrap(Arc::new(storage));
    let context = ctx();
    let harness = Harness::open(storage, HarnessOptions::new(registry, models), &context).await?;
    let root = harness.root(root_options(), &context).await?;
    let commits_before = tally.snapshot().0;
    let started = Instant::now();
    root.submit(SubmissionDraft::input("go"), &context)
        .await?
        .wait(&context)
        .await?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_tool_succeeded(&root, if codemode { "codemode" } else { "plain" }).await?;
    let commits = tally.snapshot().0 - commits_before;
    harness.close(&context).await?;
    Ok((ms, commits))
}

#[tokio::main]
async fn main() -> Result<()> {
    for (shape, sequential) in [("sequential", true), ("parallel", false)] {
        for n in sizes(&[100, 1000], &[20, 50]) {
            let (plain_ms, plain_commits) = run(false, sequential, n).await?;
            let (code_ms, code_commits) = run(true, sequential, n).await?;
            let per = |value: f64| format!("{:.2}", value / n as f64);
            println!(
                "{shape} n={n}: plain {plain_ms:.0} ms ({} ms, {} commits/call), codemode {code_ms:.0} ms ({} ms, {} commits/call)",
                per(plain_ms),
                per(plain_commits as f64),
                per(code_ms),
                per(code_commits as f64)
            );
        }
    }
    Ok(())
}
