//! Tool output benchmark: drives the real tool task, adaptive progress throttle, and `pi.live` commits.
//!
//!   cargo bench -p pi-durable --bench tool_output          (PI_DURABLE_BENCH_QUICK=1 for the small scale)
//!
//! Each scenario runs in its own child process (this binary, given the scenario as JSON) so peak RSS is its own.
//! Rate scenarios write output for a fixed time, then close the Harness mid-round (persistent backends) to measure
//! replay of the round's delta chain on reopen, and finish the run to measure the stored size after the round's base
//! and reclamation. Throughput scenarios push 1 GiB (16 MiB in quick mode).
mod support;
use pi_chord::{
    delta::{Op, Seg},
    output::{OutputLimits, Retain},
};
use pi_durable::harness::EnvironmentFactory;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::tools::{BashToolOptions, create_bash_tool};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use support::*;

const LINE: &str = "The quick brown fox jumps over the lazy dog. Unique identifier";

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Scenario {
    Rate {
        backend: String,
        rate: String,
        retain: String,
        tools: usize,
        seconds: Option<u64>,
    },
    Firehose {
        backend: String,
        retain: String,
        repetitive: bool,
    },
    Cat {
        backend: String,
    },
}
impl Scenario {
    fn backend(&self) -> &str {
        match self {
            Scenario::Rate { backend, .. }
            | Scenario::Firehose { backend, .. }
            | Scenario::Cat { backend } => backend,
        }
    }
    fn label(&self) -> String {
        match self {
            Scenario::Rate {
                rate,
                retain,
                tools,
                seconds,
                ..
            } => {
                format!(
                    "{rate} {retain} x{tools}{}",
                    seconds.map_or(String::new(), |s| format!(" {s}s"))
                )
            }
            Scenario::Firehose {
                retain, repetitive, ..
            } => {
                format!(
                    "api.output {} {retain}{}",
                    total_label(),
                    if *repetitive { " repetitive" } else { "" }
                )
            }
            Scenario::Cat { .. } => format!("bash cat {}", total_label()),
        }
    }
}
fn big_size() -> u64 {
    if quick() { 16 << 20 } else { 1 << 30 }
}
fn total_label() -> &'static str {
    if quick() { "16 MiB" } else { "1 GiB" }
}
fn rate_seconds() -> u64 {
    if quick() { 1 } else { 3 }
}
fn big_file() -> PathBuf {
    std::env::temp_dir().join(format!(
        "pi-durable-bench-{}.txt",
        total_label().replace(' ', "")
    ))
}
/// Output one tool writes per tick (lines, pause in ms).
fn rate_of(rate: &str) -> (usize, u64) {
    match rate {
        // A line every 200 ms, like a slow build step.
        "low" => (1, 200),
        // Bursts of 25 lines every 50 ms (~40 KB/s), like a compiler or test run.
        "normal" => (25, 50),
        // Continuous 64 KiB chunks, yielding to the runtime between them.
        _ => (800, 0),
    }
}
fn lines(prefix: &str, first: usize, count: usize) -> String {
    (first..first + count)
        .map(|index| format!("{prefix} {index}: {LINE}\n"))
        .collect()
}
fn ensure_big_file() -> std::io::Result<()> {
    use std::io::Write;
    let path = big_file();
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == big_size()) {
        return Ok(());
    }
    // Unique lines, written in blocks and cut at exactly the target size.
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let (mut index, mut written) = (0usize, 0u64);
    while written < big_size() {
        let mut block = String::new();
        while block.len() < 4 << 20 {
            index += 1;
            block.push_str(&format!("Line {index}: {LINE}: {:x}\n", index * 7919));
        }
        let bytes = &block.as_bytes()[..(big_size() - written).min(block.len() as u64) as usize];
        file.write_all(bytes)?;
        written += bytes.len() as u64;
    }
    file.flush()
}

async fn open_storage(backend: &str, path: &Path) -> Result<Arc<dyn Storage>> {
    Ok(match backend {
        "memory" => Arc::new(MemoryStorage::new()),
        "sqlite" => {
            Arc::new(open_native_sqlite_storage(path, NativeSqliteStorageOptions::default()).await?)
        }
        _ => Arc::new(JsonlStorage::open_native(path, &ctx(), Default::default()).await?),
    })
}
fn footprint(backend: &str, path: &Path) -> usize {
    if backend == "sqlite" {
        sqlite_size(path)
    } else {
        dir_size(path)
    }
}
fn peak_rss_mib() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    // macOS reports bytes, Linux kibibytes.
    let bytes = if cfg!(target_os = "macos") {
        usage.ru_maxrss as u64
    } else {
        usage.ru_maxrss as u64 * 1024
    };
    bytes >> 20
}
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

static STOP: AtomicBool = AtomicBool::new(false);

async fn run_scenario(scenario: Scenario) -> Result<serde_json::Value> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let backend = scenario.backend().to_string();
    let path = directory.path().join(if backend == "sqlite" {
        "session.sqlite"
    } else {
        "session"
    });
    let (models, faux) = faux(fast());
    let registry = registry();
    let tool_count = if let Scenario::Rate { tools, .. } = &scenario {
        *tools
    } else {
        1
    };
    let mut extension = Ext::new("bench");
    match &scenario {
        Scenario::Cat { .. } => extension
            .tools
            .push(Arc::new(create_bash_tool(BashToolOptions::default()))),
        _ => {
            let (retain, rate, persistent_rate, rate_scenario, repetitive) = match &scenario {
                Scenario::Rate {
                    retain,
                    rate,
                    backend,
                    ..
                } => (
                    retain.clone(),
                    rate.clone(),
                    backend != "memory",
                    true,
                    false,
                ),
                Scenario::Firehose {
                    retain, repetitive, ..
                } => (
                    retain.clone(),
                    "high".to_string(),
                    false,
                    false,
                    *repetitive,
                ),
                Scenario::Cat { .. } => unreachable!(),
            };
            let mut emit = tool(
                "emit",
                "",
                r#"{"type":"object","properties":{}}"#,
                move |_, api, context| {
                    let rate = rate.clone();
                    Box::pin(async move {
                        let (per_tick, pause_ms) = rate_of(&rate);
                        let mut counter = 0usize;
                        let mut written = 0u64;
                        // The firehose reuses one prebuilt chunk so it measures the Harness, not string building.
                        let prebuilt =
                            (!rate_scenario).then(|| lines(&api.call_id.to_string(), 0, per_tick));
                        // Repetitive: identical lines with one numbered line per chunk, so the tail window keeps sliding.
                        let repeated = "y\n".repeat(32 * 1024 - 8);
                        while !STOP.load(Ordering::SeqCst)
                            && (rate_scenario || written < big_size())
                        {
                            let chunk = if repetitive {
                                format!("{repeated}#{counter}\n")
                            } else {
                                prebuilt.clone().unwrap_or_else(|| {
                                    lines(&api.call_id.to_string(), counter, per_tick)
                                })
                            };
                            counter += per_tick;
                            written += chunk.len() as u64;
                            api.output(chunk, None)?;
                            if pause_ms == 0 {
                                tokio::task::yield_now().await;
                            } else {
                                tokio::time::sleep(std::time::Duration::from_millis(pause_ms))
                                    .await;
                            }
                        }
                        // A persistent rate scenario stays in flight until close, so reopen replays the round.
                        if rate_scenario && persistent_rate {
                            if let Some(signal) = context.abort_signal() {
                                signal.cancelled().await;
                            }
                            return Err(bench_err("aborted"));
                        }
                        Ok(ToolExecutionResult::default())
                    })
                },
            );
            emit.output_limits = OutputLimits {
                max_bytes: 50 * 1024,
                max_lines: 2000,
                retain: if retain == "tail" {
                    Retain::Tail
                } else {
                    Retain::Head
                },
            };
            extension.tools.push(Arc::new(emit));
        }
    }
    install(&registry, extension);
    let calls: Vec<_> = if matches!(scenario, Scenario::Cat { .. }) {
        vec![(
            "bash",
            json!({"command": format!("cat {}", big_file().display())}),
            "c0".to_string(),
        )]
    } else {
        (0..tool_count)
            .map(|index| ("emit", json!({}), format!("c{index}")))
            .collect()
    };
    let message = pi_ai::providers::faux::faux_assistant_message(
        calls
            .iter()
            .map(|(name, args, id)| {
                pi_ai::providers::faux::faux_tool_call(
                    *name,
                    args.as_object().cloned().unwrap_or_default(),
                    Some(id),
                )
            })
            .collect(),
        pi_ai::providers::faux::FauxMessageOptions {
            stop_reason: Some(pi_ai::StopReason::ToolUse),
            ..Default::default()
        },
    );
    faux.set_responses(vec![message.into(), answer("done")]);

    let env_dir = directory.path().to_path_buf();
    let env: EnvironmentFactory = Arc::new(move |_, _| {
        let env_dir = env_dir.clone();
        Box::pin(async move {
            Ok(Some(
                Arc::new(pi_durable::env::NativeExecutionEnv::new(&env_dir).map_err(bench_err)?)
                    as Arc<dyn pi_durable::env::ExecutionEnv>,
            ))
        })
    });
    let context = ctx();
    let open = |storage: Arc<dyn Storage>| {
        let mut options = HarnessOptions::new(registry.clone(), models.clone());
        options.env = Some(env.clone());
        Harness::open(storage, options, &context)
    };
    let (storage, tally) = Counted::wrap(open_storage(&backend, &path).await?);
    let mut harness = open(storage).await?;
    let root = harness.root(root_options(), &context).await?;
    let live_commits = Arc::new(AtomicU64::new(0));
    let live_op_bytes = Arc::new(AtomicU64::new(0));
    let output_sets = Arc::new(AtomicU64::new(0));
    let observe = |target: &Harness| -> Result<Subscription> {
        let (commits, bytes, sets) = (
            live_commits.clone(),
            live_op_bytes.clone(),
            output_sets.clone(),
        );
        target.subscribe_commits(move |publication, _| {
            for change in &publication.changes {
                let CommitChange::Document(DocumentCommitChange::Document { record, ops, .. }) =
                    change
                else {
                    continue;
                };
                if !record.create.kind.equals_str("pi.live") {
                    continue;
                }
                commits.fetch_add(1, Ordering::SeqCst);
                bytes.fetch_add(
                    serde_json::to_string(ops.as_ref()).map_or(0, |text| text.len()) as u64,
                    Ordering::SeqCst,
                );
                for op in ops.iter() {
                    if let Op::Set(path, _) = op
                        && matches!(path.last(), Some(Seg::Key(key)) if key.equals_str("output"))
                    {
                        sets.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            Ok(())
        })
    };
    let mut subscription = observe(&harness)?;
    let started = Instant::now();
    let submission = root.submit(SubmissionDraft::input("go"), &context).await?;
    let mut mid_round = None;
    let mut replay_ms = None;
    if let Scenario::Rate { seconds, .. } = &scenario {
        tokio::time::sleep(std::time::Duration::from_secs(
            seconds.unwrap_or_else(rate_seconds),
        ))
        .await;
        STOP.store(true, Ordering::SeqCst);
        if backend != "memory" {
            // Let the last progress commit land, then close mid-round and time replay on reopen.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            mid_round = Some(footprint(&backend, &path) / 1024);
            drop(subscription);
            harness.close(&context).await?;
            let reopened = Instant::now();
            let (storage, _) = Counted::wrap(open_storage(&backend, &path).await?);
            harness = open(storage).await?;
            harness.snapshot(&LIVE_DOC, root.id, &context).await?;
            subscription = observe(&harness)?;
            replay_ms = Some(reopened.elapsed().as_millis());
            harness.resume()?;
            harness
                .submission(submission.id, &context)
                .await?
                .ok_or_else(|| bench_err("the submission vanished"))?
                .wait(&context)
                .await?;
        } else {
            submission.wait(&context).await?;
        }
    } else {
        submission.wait(&context).await?;
    }
    let ms = started.elapsed().as_millis();
    let final_kib = (backend != "memory").then(|| footprint(&backend, &path) / 1024);
    drop(subscription);
    harness.close(&context).await?;
    let mut latencies = tally.latencies.lock().clone();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| {
        latencies
            .get((latencies.len() as f64 * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    Ok(json!({
        "ms": ms,
        "liveCommits": live_commits.load(Ordering::SeqCst),
        "liveOpKiB": live_op_bytes.load(Ordering::SeqCst) / 1024,
        "outputSets": output_sets.load(Ordering::SeqCst),
        "commitP50Ms": round(at(0.5)),
        "commitP99Ms": round(at(0.99)),
        "midRoundKiB": mid_round,
        "replayMs": replay_ms,
        "finalKiB": final_kib,
        "peakRssMiB": peak_rss_mib(),
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(encoded) = std::env::args().nth(1).filter(|arg| arg.starts_with('{')) {
        let scenario: Scenario = serde_json::from_str(&encoded).map_err(bench_err)?;
        let result = run_scenario(scenario).await?;
        println!("{result}");
        return Ok(());
    }
    let mut scenarios = Vec::new();
    for backend in ["memory", "sqlite", "jsonl"] {
        for rate in ["low", "normal", "high"] {
            for retain in ["head", "tail"] {
                for tools in if quick() { vec![1] } else { vec![1, 4] } {
                    scenarios.push(Scenario::Rate {
                        backend: backend.into(),
                        rate: rate.into(),
                        retain: retain.into(),
                        tools,
                        seconds: None,
                    });
                }
            }
        }
        for retain in ["head", "tail"] {
            scenarios.push(Scenario::Firehose {
                backend: backend.into(),
                retain: retain.into(),
                repetitive: false,
            });
        }
        // Identical lines: how often Chord's bounded overlap search falls back to writing the whole tail window.
        scenarios.push(Scenario::Firehose {
            backend: backend.into(),
            retain: "tail".into(),
            repetitive: true,
        });
        // One long round: the delta chain a chatty tail tool builds before the round's base.
        if backend != "memory" {
            scenarios.push(Scenario::Rate {
                backend: backend.into(),
                rate: "high".into(),
                retain: "tail".into(),
                tools: 1,
                seconds: Some(if quick() { 3 } else { 30 }),
            });
        }
        scenarios.push(Scenario::Cat {
            backend: backend.into(),
        });
    }
    ensure_big_file().map_err(bench_err)?;
    let exe = std::env::current_exe().map_err(bench_err)?;
    println!("scenario | backend | metrics");
    for scenario in scenarios {
        let output = std::process::Command::new(&exe)
            .arg(serde_json::to_string(&scenario).map_err(bench_err)?)
            .output()
            .map_err(bench_err)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.lines().last().unwrap_or("");
        if !output.status.success() || line.is_empty() {
            return Err(bench_err(format!(
                "{} {} failed: {}",
                scenario.label(),
                scenario.backend(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        println!("{} | {} | {line}", scenario.label(), scenario.backend());
    }
    Ok(())
}
