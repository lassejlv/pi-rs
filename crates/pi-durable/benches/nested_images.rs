//! Nested image storage benchmark. One model-issued `script` call, as a code mode call would, loops `count` times: a
//! nested `read_image` call returns a unique base64 image of `bytes` characters, then a nested `classify` call reads
//! it. The `inline` variant passes the image data in `classify`'s arguments; the `reference` variant passes the
//! `taskId` of the `read_image` result, whose value is the image, and `classify` resolves it from its caller's
//! nested-call document. Reports commits, wall time, logical bytes written (JSON of the storage writes) by write
//! type, the stored size before `script` returns, after the run settles, after close, and after reopening, the heap
//! (counting allocator: peak during the run; live bytes once the run settles, after `wait_for_idle()`, and after
//! reopening; all over the live bytes before opening), the number of nested task records, and the size of the
//! largest one.
//!
//!   cargo bench -p pi-durable --bench nested_images [count]    (PI_DURABLE_BENCH_QUICK=1 for small images)
mod support;
use base64::Engine;
use pi_durable::harness::{
    registry::ToolCaller,
    tool::{NESTED_CALL_DOC, NestedCallOptions},
};
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::*;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use support::*;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Sqlite,
    Jsonl,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Inline,
    Reference,
}
#[derive(Clone, Copy)]
struct Scenario {
    backend: Backend,
    variant: Variant,
    count: usize,
    bytes: usize,
}
struct Result_ {
    commits: u64,
    ms: u128,
    written: std::collections::BTreeMap<String, usize>,
    stored_live: usize,
    stored_run: usize,
    stored_closed: usize,
    stored_reopened: usize,
    peak: isize,
    settled: isize,
    idle: isize,
    reopened: isize,
    nested_tasks: usize,
    largest_task: usize,
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

static SERIAL: AtomicU64 = AtomicU64::new(1);
/// Unique base64 of `chars` characters: pseudo-random bytes, never repeated across calls.
fn unique_image(chars: usize) -> String {
    let mut state = SERIAL
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        | 1;
    let bytes: Vec<u8> = (0..chars / 4 * 3)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn extension_for(scenario: Scenario, stored_live: Arc<AtomicUsize>, directory: PathBuf) -> Ext {
    let bytes = scenario.bytes;
    let read_image = tool(
        "read_image",
        "Read an image",
        r#"{"type":"object","properties":{"n":{"type":"number"}},"required":["n"]}"#,
        move |_, _, _| {
            Box::pin(async move {
                // One image, so programs get it as an image block. Unique per call.
                let block = object(
                    &json!({"type": "image", "data": unique_image(bytes), "mimeType": "image/png"})
                        .to_string(),
                );
                Ok(ToolExecutionResult {
                    output: Some(Arc::new(vec![Arc::new(block)])),
                    ..Default::default()
                })
            })
        },
    );
    let classify = tool(
        "classify",
        "Classify images",
        r#"{"type":"object","properties":{"images":{"type":"array","items":{"type":"object","properties":{"type":{"type":"string"},"mimeType":{"type":"string"},"data":{"type":"string"},"source":{"type":"object","properties":{"taskId":{"type":"number"}}}},"required":["type","mimeType"]}}},"required":["images"]}"#,
        move |args, api, context| {
            Box::pin(async move {
                let args = sj(args.value());
                for image in args["images"].as_array().cloned().unwrap_or_default() {
                    let mut data = image["data"].as_str().map(str::to_string);
                    if let Some(source) = image["source"]["taskId"].as_i64() {
                        // The caller is the nested call's `parent`; its nested-call document holds the
                        // referenced result.
                        let me = api
                            .get_task::<JsonValue>(
                                TypedTaskId::from_untyped(api.task_id().untyped()),
                                &context,
                            )
                            .await?
                            .ok_or_else(|| bench_err("the classify task is missing"))?;
                        let me = sj(&pi_chord::json::to_value(&me.info.input).map_err(bench_err)?);
                        if me["kind"] != "nested" {
                            return Err(bench_err(
                                "classify resolves references only as a nested call",
                            ));
                        }
                        let parent = TaskId::from_raw(me["parent"].as_i64().unwrap_or(0));
                        let source = api
                            .get_task::<JsonValue>(
                                TypedTaskId::from_untyped(TaskId::from_raw(source)),
                                &context,
                            )
                            .await?
                            .ok_or_else(|| bench_err("the referenced task is missing"))?;
                        let source =
                            sj(&pi_chord::json::to_value(&source.info.input).map_err(bench_err)?);
                        if source["kind"] != "nested" {
                            return Err(bench_err("a reference names a nested call"));
                        }
                        let key = JsonString::from(source["key"].as_str().unwrap_or(""));
                        let stored = api
                            .snapshot_family(&NESTED_CALL_DOC, parent, key, &context)
                            .await?;
                        let block = stored.and_then(|state| {
                            state
                                .result
                                .as_ref()
                                .and_then(|result| result.structured_output.clone())
                        });
                        data =
                            block.and_then(|block| sj(&block)["data"].as_str().map(str::to_string));
                    }
                    let length = data.as_ref().map(String::len);
                    if length != Some(bytes) {
                        return Err(bench_err(format!("Image has {length:?} characters")));
                    }
                }
                Ok(text_result("ok"))
            })
        },
    );
    let mut script = tool(
        "script",
        "Read and classify images",
        r#"{"type":"object","properties":{"count":{"type":"number"}},"required":["count"]}"#,
        move |args, api, context| {
            let (stored_live, directory) = (stored_live.clone(), directory.clone());
            Box::pin(async move {
                let count = sj(args.value())["count"].as_u64().unwrap_or(0);
                for n in 0..count {
                    let read = api
                        .execute_tool(
                            "read_image",
                            object(&json!({"n": n}).to_string()),
                            &context,
                            NestedCallOptions {
                                key: Some(format!("read{n}").into()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    let image = sj(&read
                        .structured_output
                        .clone()
                        .ok_or_else(|| bench_err("no image result"))?);
                    let argument: Value = if scenario.variant == Variant::Inline {
                        json!({"type": "image", "data": image["data"], "mimeType": image["mimeType"]})
                    } else {
                        json!({"type": "image", "source": {"taskId": read.task_id.get()}, "mimeType": image["mimeType"]})
                    };
                    let classified = api
                        .execute_tool(
                            "classify",
                            object(&json!({"images": [argument]}).to_string()),
                            &context,
                            NestedCallOptions {
                                key: Some(format!("classify{n}").into()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    if classified.is_error {
                        return Err(bench_err(format!(
                            "classify {n} failed: {:?}",
                            classified.diagnostics
                        )));
                    }
                }
                stored_live.store(dir_size(&directory), Ordering::SeqCst);
                Ok(text_result(&format!("classified {count}")))
            })
        },
    );
    script.callers = Some(vec![ToolCaller::Model]);
    let mut extension = Ext::new("bench");
    extension.tools.push(Arc::new(read_image));
    extension.tools.push(Arc::new(classify));
    extension.tools.push(Arc::new(script));
    extension
}

async fn run_scenario(scenario: Scenario) -> Result<Result_> {
    let directory = tempfile::tempdir().map_err(bench_err)?;
    let (models, faux) = faux(fast());
    let stored_live = Arc::new(AtomicUsize::new(0));
    let registry = registry();
    install(
        &registry,
        extension_for(
            scenario,
            stored_live.clone(),
            directory.path().to_path_buf(),
        ),
    );
    let base = live_bytes() as isize;
    let (storage, tally) = Counted::wrap(open(scenario.backend, directory.path()).await?);
    let context = ctx();
    let harness = Harness::open(
        storage,
        HarnessOptions::new(registry.clone(), models.clone()),
        &context,
    )
    .await?;
    let root = harness.root(root_options(), &context).await?;
    faux.set_responses(vec![
        call("script", json!({"count": scenario.count}), "s"),
        answer("done"),
    ]);
    let (commits_before, written_before) = tally.snapshot();
    reset_peak();
    let started = Instant::now();
    let settled = root
        .submit(SubmissionDraft::input("run"), &context)
        .await?
        .wait(&context)
        .await?;
    if settled.as_record().state.status() != SubmissionStatus::Done {
        return Err(bench_err("run did not finish"));
    }
    // `done` only says the model answered; the script's result says whether it ran through.
    assert_tool_succeeded(&root, "script").await?;
    let ms = started.elapsed().as_millis();
    let peak = peak_bytes() as isize - base;
    let settled_heap = live_bytes() as isize - base;
    harness.wait_for_idle(&context).await?;
    let idle = live_bytes() as isize - base;
    let (commits, written) = tally.snapshot();
    if std::env::var("PI_DURABLE_BENCH_VERBOSE").is_ok() {
        let latencies = tally.latencies.lock();
        eprintln!(
            "commit time: total {:.0} ms over {} commits, max {:.0} ms (run {} ms)",
            latencies.iter().sum::<f64>(),
            latencies.len(),
            latencies.iter().cloned().fold(0.0, f64::max),
            ms
        );
    }
    let stored_run = dir_size(directory.path());
    harness.close(&context).await?;
    let stored_closed = dir_size(directory.path());

    let reopened_storage = open(scenario.backend, directory.path()).await?;
    let reopened = Harness::open(
        reopened_storage.clone(),
        HarnessOptions::new(registry, models),
        &context,
    )
    .await?;
    let reopened_heap = live_bytes() as isize - base;
    let stored_reopened = dir_size(directory.path());
    let (mut nested_tasks, mut largest_task) = (0, 0);
    let mut cursor: Option<Cursor> = None;
    loop {
        let query = TaskQuery {
            kind: Some("pi.tool".into()),
            ..Default::default()
        };
        let page = reopened_storage
            .scan_tasks(&query, 50, cursor.as_ref(), &context)
            .await?;
        for record in &page.items {
            let value = serde_json::to_value(record).map_err(bench_err)?;
            if value["input"]["kind"] == "nested" {
                nested_tasks += 1;
            }
            largest_task = largest_task.max(value.to_string().len());
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    reopened.close(&context).await?;
    Ok(Result_ {
        commits: commits - commits_before,
        ms,
        written: written
            .iter()
            .map(|(k, v)| (k.clone(), v - written_before.get(k).copied().unwrap_or(0)))
            .collect(),
        stored_live: stored_live.load(Ordering::SeqCst),
        stored_run,
        stored_closed,
        stored_reopened,
        peak,
        settled: settled_heap,
        idle,
        reopened: reopened_heap,
        nested_tasks,
        largest_task,
    })
}

fn smib(bytes: isize) -> String {
    format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
}

#[tokio::main]
async fn main() -> Result<()> {
    let counts = sizes(&[50, 200], &[10]);
    let counts = match std::env::args()
        .skip(1)
        .find_map(|arg| arg.parse::<usize>().ok())
    {
        Some(count) => vec![count],
        None => counts,
    };
    let bytes = if quick() { 256 * 1024 } else { 1024 * 1024 };
    println!(
        "Images: {} KiB base64 each, read then classified sequentially in one script call",
        bytes / 1024
    );
    println!(
        "backend | variant   | count | commits, ms | written: all (task / doc create / doc change / entry) | stored: live / run / closed / reopened | heap: peak; settled / idle / reopened | nested tasks, largest task"
    );
    for count in counts {
        for backend in [Backend::Sqlite, Backend::Jsonl] {
            for variant in [Variant::Inline, Variant::Reference] {
                let label = format!(
                    "{}-{}",
                    if backend == Backend::Sqlite {
                        "sqlite"
                    } else {
                        "jsonl"
                    },
                    if variant == Variant::Inline {
                        "inline"
                    } else {
                        "reference"
                    }
                );
                if std::env::var("PI_DURABLE_BENCH_ONLY").is_ok_and(|only| only != label) {
                    continue;
                }
                let result = run_scenario(Scenario {
                    backend,
                    variant,
                    count,
                    bytes,
                })
                .await?;
                let get = |kind: &str| result.written.get(kind).copied().unwrap_or(0);
                println!(
                    "{:<7} | {:<9} | {:<5} | {}, {} ms | {} ({} / {} / {} / {}) | {} / {} / {} / {} | {}; {} / {} / {} | {}, {}",
                    if backend == Backend::Sqlite {
                        "sqlite"
                    } else {
                        "jsonl"
                    },
                    if variant == Variant::Inline {
                        "inline"
                    } else {
                        "reference"
                    },
                    count,
                    result.commits,
                    result.ms,
                    mib(total(&result.written)),
                    mib(get("task")),
                    mib(get("document.create")),
                    mib(get("document.change")),
                    mib(get("entry")),
                    mib(result.stored_live),
                    mib(result.stored_run),
                    mib(result.stored_closed),
                    mib(result.stored_reopened),
                    smib(result.peak),
                    smib(result.settled),
                    smib(result.idle),
                    smib(result.reopened),
                    result.nested_tasks,
                    kib(result.largest_task),
                );
            }
        }
    }
    Ok(())
}
