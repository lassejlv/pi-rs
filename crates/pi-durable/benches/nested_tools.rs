//! Nested tool call storage benchmark. Per run, one model-issued tool makes `calls` parallel nested calls of `blob`,
//! each streaming `bytes` of output in 8 chunks `gap_ms` apart; at depth 2 it makes them through `calls / 10`
//! parallel `fan` calls of 10 each. With `progress: false` the nested calls commit no running output. Reports
//! commits, logical bytes written (JSON of the storage writes) by write type, the stored size after each run, and the
//! heap (counting allocator): its peak during a run and what stays after a run, both over the live bytes before the
//! first run. A 2 ms gap is a fast tool such as `read`: it ends before a second progress commit. A 50 ms gap streams
//! for 400 ms, as a short `bash` command does, and commits progress every 100 ms.
//!
//!   cargo bench -p pi-durable --bench nested_tools [streaming|nesting]    (PI_DURABLE_BENCH_QUICK=1 for small runs)
mod support;
use pi_durable::harness::tool::NestedCallOptions;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde_json::json;
use std::{path::Path, sync::Arc, time::Instant};
use support::*;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const CHUNKS: usize = 8;
/// Calls each inner `fan` makes at depth 2.
const FAN_OUT: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Sqlite,
    Jsonl,
}
#[derive(Clone, Copy)]
struct Scenario {
    backend: Backend,
    calls: usize,
    depth: usize,
    bytes: usize,
    gap_ms: u64,
    /// Whether nested calls stream their running output to `pi.live`.
    progress: bool,
    runs: usize,
}
struct Run {
    commits: u64,
    all: usize,
    tasks: usize,
    changes: usize,
    creates: usize,
    stored: usize,
    ms: u128,
    peak: isize,
    held: isize,
}

async fn open(backend: Backend, directory: &Path) -> Result<Arc<dyn Storage>> {
    Ok(match backend {
        Backend::Sqlite => Arc::new(
            open_native_sqlite_storage(
                directory.join("session.sqlite"),
                NativeSqliteStorageOptions::default(),
            )
            .await?,
        ),
        Backend::Jsonl => {
            Arc::new(JsonlStorage::open_native(directory, &ctx(), Default::default()).await?)
        }
    })
}

fn fan_out(name: &str, target: &'static str, progress: bool) -> NativeTool {
    tool(
        name,
        &format!("Call {target} in parallel"),
        r#"{"type":"object","properties":{"count":{"type":"number"}},"required":["count"]}"#,
        move |args, api, context| {
            Box::pin(async move {
                let count = sj(args.value())["count"].as_u64().unwrap_or(0);
                let results = futures::future::join_all((0..count).map(|n| {
                    api.execute_tool(
                        target,
                        object(&json!({"n": n, "count": FAN_OUT}).to_string()),
                        &context,
                        NestedCallOptions {
                            progress: if progress { None } else { Some(false) },
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
    )
}

async fn run_scenario(scenario: Scenario) -> Result<(Vec<Run>, usize)> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let (models, faux) = faux(fast());
    let registry = registry();
    let chunk = format!("{}\n", "x".repeat(scenario.bytes / CHUNKS - 1));
    let gap = scenario.gap_ms;
    // Streams its output and returns none, so its result is the retained output, as bash's is.
    let blob = tool(
        "blob",
        "Stream a blob",
        r#"{"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}"#,
        move |_, api, _| {
            let chunk = chunk.clone();
            Box::pin(async move {
                for _ in 0..CHUNKS {
                    api.output(chunk.as_str(), None)?;
                    tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
                }
                Ok(ToolExecutionResult::default())
            })
        },
    );
    let mut extension = Ext::new("bench");
    extension.tools.push(Arc::new(blob));
    extension
        .tools
        .push(Arc::new(fan_out("fan", "blob", scenario.progress)));
    extension
        .tools
        .push(Arc::new(fan_out("fan2", "fan", scenario.progress)));
    install(&registry, extension);

    let (storage, tally) = Counted::wrap(open(scenario.backend, directory.path()).await?);
    let context = ctx();
    let harness = Harness::open(storage, HarnessOptions::new(registry, models), &context).await?;
    let root = harness.root(root_options(), &context).await?;
    let top = if scenario.depth == 1 {
        scenario.calls
    } else {
        scenario.calls / FAN_OUT
    };
    let base = live_bytes() as isize;
    let mut runs = Vec::new();
    for run in 0..scenario.runs {
        faux.set_responses(vec![
            call(
                if scenario.depth == 1 { "fan" } else { "fan2" },
                json!({"count": top}),
                &format!("r{run}"),
            ),
            answer("done"),
        ]);
        let (commits_before, written_before) = tally.snapshot();
        reset_peak();
        let started = Instant::now();
        let settled = root
            .submit(
                SubmissionDraft::input(format!("run {run}").as_str()),
                &context,
            )
            .await?
            .wait(&context)
            .await?;
        if settled.as_record().state.status() != SubmissionStatus::Done {
            return Err(bench_err(format!("run {run}: not done")));
        }
        assert_tool_succeeded(&root, &format!("run {run}")).await?;
        harness.wait_for_idle(&context).await?;
        let ms = started.elapsed().as_millis();
        let peak = peak_bytes() as isize - base;
        let (commits, written) = tally.snapshot();
        let stored = dir_size(directory.path());
        runs.push(Run {
            commits: commits - commits_before,
            all: total(&written) - total(&written_before),
            tasks: delta(&written_before, &written, "task"),
            changes: delta(&written_before, &written, "document.change"),
            creates: delta(&written_before, &written, "document.create"),
            stored,
            ms,
            peak,
            held: live_bytes() as isize - base,
        });
    }
    harness.close(&context).await?;
    Ok((runs, dir_size(directory.path())))
}

fn signed_mib(bytes: isize) -> String {
    format!("{:.2} MiB", bytes as f64 / 1024.0 / 1024.0)
}

async fn report(title: &str, scenarios: Vec<Scenario>) -> Result<()> {
    println!("\n{title}");
    println!(
        "backend | calls | depth | output | gap   | stream | run 1: commits, ms, written (all; tasks / doc changes / doc creates) | stored after each run | after close | heap: run 1 peak; held after each run"
    );
    for scenario in scenarios {
        let (runs, closed) = run_scenario(scenario).await?;
        let first = &runs[0];
        println!(
            "{:<6} | {:<5} | {:<5} | {:<6} | {:<5} | {:<6} | {}, {} ms, {} ({} / {} / {}) | {} | {} | {}; {}",
            if scenario.backend == Backend::Sqlite {
                "sqlite"
            } else {
                "jsonl"
            },
            scenario.calls,
            scenario.depth,
            format!("{} KiB", scenario.bytes / 1024),
            format!("{} ms", scenario.gap_ms),
            if scenario.progress { "yes" } else { "no" },
            first.commits,
            first.ms,
            mib(first.all),
            mib(first.tasks),
            mib(first.changes),
            mib(first.creates),
            runs.iter()
                .map(|run| mib(run.stored))
                .collect::<Vec<_>>()
                .join(" / "),
            mib(closed),
            signed_mib(first.peak),
            runs.iter()
                .map(|run| signed_mib(run.held))
                .collect::<Vec<_>>()
                .join(" / "),
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let section = std::env::args()
        .skip(1)
        .find(|arg| arg == "streaming" || arg == "nesting");
    let fast_mode = quick();
    let runs = if fast_mode { 2 } else { 3 };
    if section
        .as_deref()
        .is_none_or(|section| section == "streaming")
    {
        let mut scenarios = Vec::new();
        for backend in [Backend::Sqlite, Backend::Jsonl] {
            for bytes in if fast_mode {
                vec![1024]
            } else {
                vec![1024, 16 * 1024]
            } {
                for gap_ms in [2, 50] {
                    for progress in [true, false] {
                        let calls = if fast_mode { 20 } else { 200 };
                        scenarios.push(Scenario {
                            backend,
                            calls,
                            depth: 1,
                            bytes,
                            gap_ms,
                            progress,
                            runs,
                        });
                    }
                }
            }
        }
        report("Streaming: calls with and without progress", scenarios).await?;
    }
    if section
        .as_deref()
        .is_none_or(|section| section == "nesting")
    {
        let mut scenarios = Vec::new();
        for backend in [Backend::Sqlite, Backend::Jsonl] {
            for calls in if fast_mode {
                vec![20, 100]
            } else {
                vec![200, 2000]
            } {
                for depth in [1, 2] {
                    scenarios.push(Scenario {
                        backend,
                        calls,
                        depth,
                        bytes: 1024,
                        gap_ms: 2,
                        progress: false,
                        runs,
                    });
                }
            }
        }
        report(
            "Nesting: fast calls without progress, at depth 1 and 2",
            scenarios,
        )
        .await?;
    }
    Ok(())
}
