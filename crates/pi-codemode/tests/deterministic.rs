//! Port of `packages/codemode/test/deterministic.test.ts`: `start()` runs a script
//! deterministically with the caller answering its calls, so a durable host can replay it after a
//! restart. Every test runs on the thread sandbox and on the remote sandbox, whose transport
//! round-trips every message through JSON.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_codemode::*;
use serde_json::{Value, json};

const BUDGET: u64 = 50_000;
/// 2026-01-15T10:00:00Z
const STARTED_AT: f64 = 1_768_471_200_000.0;

/// The first `Math.random()` with seed 1.
const PINNED_FIRST_RANDOM: f64 = 0.34227450605335275;
/// The first two `Math.random()` values with seed 0.
const PINNED_SEED_ZERO: [f64; 2] = [0.6711372530266764, 0.7258461452833667];

/// A remote that round-trips through JSON; `delay_first` holds the reply to result 1 back, so later
/// results reach the VM before it.
struct DelayingRemote {
    delay_first: bool,
}

impl CodemodeRemote for DelayingRemote {
    fn execute(&self, request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        let delay_first = self.delay_first;
        let wrapped: CodemodeRemoteExchange = Arc::new(move |messages: Vec<CodemodeRemoteMessage>| {
            let reply = exchange(roundtrip(&messages));
            Box::pin(async move {
                let reply = reply.await.map(|reply| roundtrip(&reply));
                if delay_first
                    && let Ok(CodemodeRemoteReply::Result(HostToWorkerMessage::Result { seq: Some(1), .. })) = &reply
                {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                reply
            })
        });
        let request = roundtrip(&request);
        Box::pin(async move {
            serve_codemode_remote(
                request,
                wrapped,
                CodemodeServeOptions { max_interrupt_budget: Some(BUDGET), ..Default::default() },
            )
            .await;
            Ok(())
        })
    }
}

fn remote(memory_limit_bytes: Option<u64>, delay_first: bool) -> CodemodeSandbox {
    let mut options = RemoteCodemodeSandboxOptions::new(move || Ok(Arc::new(DelayingRemote { delay_first }) as Arc<dyn CodemodeRemote>));
    options.memory_limit_bytes = memory_limit_bytes;
    CodemodeSandbox::remote(options).unwrap()
}

fn create(backend: Backend, memory_limit_bytes: Option<u64>) -> CodemodeSandbox {
    match backend {
        Backend::Thread => CodemodeSandbox::new(CodemodeSandboxOptions { memory_limit_bytes, ..Default::default() }).unwrap(),
        Backend::Remote => remote(memory_limit_bytes, false),
    }
}

struct Recorded {
    run: Option<CodemodeRun>,
    handle: CodemodeRunHandle,
    calls: Arc<Mutex<Vec<CodemodeHostCall>>>,
    idles: Arc<Mutex<Vec<(u64, u64)>>>,
    events: Arc<Mutex<Vec<String>>>,
    fingerprint: Arc<Mutex<Option<String>>>,
}

impl Recorded {
    async fn result(&mut self) -> CodemodeResult {
        self.run.take().expect("result taken once").result().await
    }

    fn calls(&self) -> Vec<CodemodeHostCall> {
        self.calls.lock().clone()
    }

    fn idles(&self) -> Vec<(u64, u64)> {
        self.idles.lock().clone()
    }
}

type Answer = Arc<dyn Fn(&CodemodeHostCall) -> CodemodeOutcome + Send + Sync>;

/// Start `code` with tools `echo` and `wait`. Every call but `wait` is answered at once, by
/// default with its argument, as result number 1, 2, ... with the clock at
/// `STARTED_AT + 1000 * seq`; `wait` is left for the test to answer.
fn start(sandbox: &CodemodeSandbox, code: &str, configure: impl FnOnce(&mut CodemodeRunOptions), answer: Option<Answer>) -> Recorded {
    let calls: Arc<Mutex<Vec<CodemodeHostCall>>> = Arc::default();
    let idles: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let fingerprint: Arc<Mutex<Option<String>>> = Arc::default();
    let seq = Arc::new(AtomicU64::new(0));
    let answer: Answer = answer.unwrap_or_else(|| Arc::new(|call: &CodemodeHostCall| CodemodeOutcome::ok(call.args.clone())));
    let (calls_, events_, seq_) = (calls.clone(), events.clone(), seq.clone());
    let mut options = CodemodeRunOptions::new(STARTED_AT, 1, move |call, handle| {
        calls_.lock().push(call.clone());
        events_.lock().push(format!("call {}", call.id));
        if call.name == "wait" {
            return;
        }
        let seq = seq_.fetch_add(1, Ordering::SeqCst) + 1;
        handle.deliver(seq, STARTED_AT + 1000.0 * seq as f64, call.id, answer(&call)).expect("deliver");
    });
    options.tools = vec![CodemodeDeclaredTool::new("echo"), CodemodeDeclaredTool::new("wait")];
    options.timeout_ms = Some(20_000.0);
    let (fingerprint_, events_) = (fingerprint.clone(), events.clone());
    options.on_started = Some(Arc::new(move |value: &str| {
        *fingerprint_.lock() = Some(value.to_string());
        events_.lock().push("started".into());
    }));
    let (idles_, events_) = (idles.clone(), events.clone());
    options.on_idle = Some(Arc::new(move |seq, issued| {
        idles_.lock().push((seq, issued));
        events_.lock().push(format!("idle {seq}"));
    }));
    configure(&mut options);
    let run = sandbox.start(code, options).expect("start");
    let handle = run.handle();
    Recorded { run: Some(run), handle, calls, idles, events, fingerprint }
}

fn plain(sandbox: &CodemodeSandbox, code: &str) -> Recorded {
    start(sandbox, code, |_| {}, None)
}

/// Call durations are wall-clock time; everything else must be the same.
fn timeless(result: &CodemodeResult) -> Value {
    let mut value = json(result);
    for call in value["calls"].as_array_mut().unwrap() {
        call.as_object_mut().unwrap().remove("durationMs");
    }
    value
}

backend_tests!(
    makes_the_same_calls_and_output_for_the_same_start_seed_and_results,
    runs_in_utc_without_weak_ref_or_finalization_registry,
    applies_results_in_seq_order_whatever_order_they_are_delivered_in,
    reports_the_fingerprint_before_any_call_and_the_calls_issued_after_each_result,
    numbers_calls_to_globals_with_tools_and_answers_errors_as_rejections,
    keeps_the_clock_monotonic_and_rejects_a_clock_value_that_is_not_finite,
    reports_the_result_the_vm_was_processing_when_the_script_ended,
    rejects_with_an_error_carrying_the_answers_properties,
    rejects_rejection_properties_that_are_not_an_object_and_tools_sharing_a_script_name,
    uses_the_script_names_the_run_passes,
    ends_with_an_error_the_script_cannot_catch_when_the_caller_aborts,
    is_finished_once_it_has_its_outcome,
    rejects_a_call_that_is_not_waiting_and_a_result_number_used_twice,
);

async fn makes_the_same_calls_and_output_for_the_same_start_seed_and_results(backend: Backend) {
    let code = r#"
        const values = [Date.now(), performance.now(), Math.random(), Math.random(), new Date().toISOString()];
        const first = await tools.echo(values);
        text(JSON.stringify([Date.now(), performance.now(), Math.random()]));
        return await tools.echo(first.concat(Math.random()));
    "#;
    let sandbox = create(backend, None);
    let mut one = plain(&sandbox, code);
    let mut two = plain(&sandbox, code);
    let (a, b) = tokio::join!(one.result(), two.result());
    assert_eq!(timeless(&a), timeless(&b));
    assert_eq!(one.calls(), two.calls());
    // The clock: startedAt, then the time of the result being processed; performance.now() counts
    // from startedAt.
    let args = one.calls()[0].args.clone().unwrap();
    assert_eq!(args[0].as_f64(), Some(STARTED_AT));
    assert_eq!(args[1].as_f64(), Some(0.0));
    assert_eq!(args[4], json!("2026-01-15T10:00:00.000Z"));
    let CodemodeOutputItem::Text { text, .. } = &a.output[0] else { panic!("text") };
    let printed: Vec<f64> = serde_json::from_str(text).unwrap();
    assert_eq!(printed[..2], [STARTED_AT + 1000.0, 1000.0]);
    // Pinned: a change here changes what replayed scripts see, so it must change the fingerprint too.
    let random = args[2].as_f64().unwrap();
    assert_eq!(random, PINNED_FIRST_RANDOM);

    // The seed decides the random values, not the start time.
    let mut later = start(&sandbox, code, |options| options.started_at = STARTED_AT + 5000.0, None);
    let mut reseeded = start(&sandbox, code, |options| options.seed = 2, None);
    tokio::join!(later.result(), reseeded.result());
    assert_eq!(later.calls()[0].args.as_ref().unwrap()[2].as_f64(), Some(random));
    assert_ne!(reseeded.calls()[0].args.as_ref().unwrap()[2].as_f64(), Some(random));
    // Seed 0 has its own values.
    let mut zero = start(&sandbox, "return [Math.random(), Math.random()]", |options| options.seed = 0, None);
    let zero = zero.result().await;
    assert_matches(&zero, json!({ "ok": true, "value": PINNED_SEED_ZERO }));
    assert!(!zero.value().unwrap().as_array().unwrap().contains(&json!(random)));
}

async fn runs_in_utc_without_weak_ref_or_finalization_registry(backend: Backend) {
    let code =
        "return [new Date(0).getHours(), new Date(0).getTimezoneOffset(), typeof WeakRef, typeof FinalizationRegistry]";
    let sandbox = create(backend, None);
    let mut recorded = plain(&sandbox, code);
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": [0, 0, "undefined", "undefined"] }));
    // execute() is unchanged: every intrinsic (and the host's timezone).
    let result = run(&sandbox, code).await;
    assert_matches(&result, json!({ "ok": true }));
    assert_eq!(result.value().unwrap()[2], json!("function"));
    assert_eq!(result.value().unwrap()[3], json!("function"));
}

async fn applies_results_in_seq_order_whatever_order_they_are_delivered_in(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = plain(
        &sandbox,
        r#"const order = [];
        await Promise.all([tools.wait("a").then(() => order.push("a")), tools.wait("b").then(() => order.push("b"))]);
        return order;"#,
    );
    wait_for(|| recorded.calls.lock().len() == 2 && recorded.idles.lock().len() == 1).await;
    let calls = recorded.calls();
    // b is result 1 and a result 2, but a arrives first.
    recorded.handle.deliver(2, STARTED_AT + 2000.0, calls[0].id, CodemodeOutcome::ok(Value::Null)).unwrap();
    recorded.handle.deliver(1, STARTED_AT + 1000.0, calls[1].id, CodemodeOutcome::ok(Value::Null)).unwrap();
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": ["b", "a"] }));
    // No idle after the last result: the script finished in that drain, and the host stopped the VM.
    assert_eq!(recorded.idles(), vec![(0, 2), (1, 2)]);
}

async fn reports_the_fingerprint_before_any_call_and_the_calls_issued_after_each_result(backend: Backend) {
    let sandbox = create(backend, Some(64 * 1024 * 1024));
    let mut recorded = plain(&sandbox, "await tools.echo(1); await Promise.all([tools.echo(2), tools.echo(3)]);");
    assert_matches(&recorded.result().await, json!({ "ok": true }));
    assert_eq!(recorded.events.lock()[..3].to_vec(), vec!["started", "call 1", "idle 0"]);
    assert_eq!(recorded.idles(), vec![(0, 1), (1, 3), (2, 3)]);
    let calls: Vec<Value> =
        recorded.calls().iter().map(|call| json!([call.id, call.target, call.name, call.args])).collect();
    assert_eq!(calls, vec![json!([1, "tool", "echo", 1]), json!([2, "tool", "echo", 2]), json!([3, "tool", "echo", 3])]);
    let fingerprint = recorded.fingerprint.lock().clone().unwrap();
    let parts: Vec<&str> = fingerprint.split(", ").collect();
    assert_eq!(parts.len(), 6, "{fingerprint}");
    assert!(parts[0].starts_with("boa "), "{fingerprint}");
    assert!(parts[1].starts_with("prelude ") && parts[1].len() == "prelude ".len() + 8, "{fingerprint}");
    assert!(parts[2].starts_with("deterministic "), "{fingerprint}");
    assert!(parts[3].starts_with("stack "), "{fingerprint}");
    assert_eq!(parts[4], "heap 67108864");
    match backend {
        Backend::Remote => assert_eq!(parts[5], format!("budget {BUDGET}")),
        Backend::Thread => assert_eq!(parts[5], "budget none"),
    }

    let mut same = plain(&sandbox, "return 1");
    let other_sandbox = create(backend, Some(32 * 1024 * 1024));
    let mut other = plain(&other_sandbox, "return 1");
    tokio::join!(same.result(), other.result());
    assert_eq!(same.fingerprint.lock().clone().unwrap(), fingerprint);
    assert_ne!(other.fingerprint.lock().clone().unwrap(), fingerprint);
}

async fn numbers_calls_to_globals_with_tools_and_answers_errors_as_rejections(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = start(
        &sandbox,
        r#"const found = await search("x", 2);
        try { await tools.echo("fail"); } catch (error) { text(error.message); }
        return found;"#,
        |options| options.globals = vec![CodemodeDeclaredGlobal { name: "search".into(), spread: true }],
        Some(Arc::new(|call: &CodemodeHostCall| {
            if call.args == Some(json!("fail")) {
                CodemodeOutcome::err("no")
            } else if call.name == "search" {
                CodemodeOutcome::ok(json!(["found"]))
            } else {
                CodemodeOutcome::ok(call.args.clone())
            }
        })),
    );
    let result = recorded.result().await;
    let calls = recorded.calls();
    assert_eq!(
        calls[0],
        CodemodeHostCall { id: 1, target: CodemodeCallTarget::Global, name: "search".into(), args: Some(json!(["x", 2])) }
    );
    assert_matches(&result, json!({ "ok": true, "value": ["found"], "output": [{ "type": "text", "text": "no" }] }));
    assert_eq!((calls[1].id, calls[1].target, calls[1].name.as_str()), (2, CodemodeCallTarget::Tool, "echo"));
    // Calls to globals count as issued calls too.
    assert_eq!(recorded.idles()[..2].to_vec(), vec![(0, 1), (1, 2)]);
    assert_matches(&result, json!({ "calls": [{ "name": "echo", "status": "error" }] }));
}

async fn keeps_the_clock_monotonic_and_rejects_a_clock_value_that_is_not_finite(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = plain(&sandbox, "const before = Date.now(); await tools.wait(1); return [before, Date.now(), performance.now()]");
    wait_for(|| recorded.calls.lock().len() == 1).await;
    let error = recorded.handle.deliver(1, f64::NAN, 1, CodemodeOutcome::ok(Value::Null)).unwrap_err();
    assert!(error.contains("not a finite number"), "{error}");
    // A resumed host whose clock is behind the run's start.
    recorded.handle.deliver(1, STARTED_AT - 2000.0, 1, CodemodeOutcome::ok(Value::Null)).unwrap();
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": [STARTED_AT, STARTED_AT, 0] }));
}

async fn reports_the_result_the_vm_was_processing_when_the_script_ended(backend: Backend) {
    let sandbox = create(backend, None);
    let mut returned = plain(&sandbox, "await tools.echo(1); return await tools.echo(2)");
    let mut thrown =
        plain(&sandbox, "await tools.echo(1); await Promise.all([tools.echo(2), tools.echo(3)]); throw new Error('x')");
    let mut first = plain(&sandbox, "return 1");
    assert_matches(&returned.result().await, json!({ "ok": true, "value": 2, "during": { "seq": 2, "issued": 2 } }));
    assert_matches(&thrown.result().await, json!({ "ok": false, "during": { "seq": 3, "issued": 3 } }));
    assert_matches(&first.result().await, json!({ "ok": true, "during": { "seq": 0, "issued": 0 } }));
    assert_eq!(run(&sandbox, "return 1").await.during, None);
}

async fn rejects_with_an_error_carrying_the_answers_properties(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = start(
        &sandbox,
        "try { await tools.echo(1); } catch (e) { return [e instanceof Error, e.message, e.name, e.structuredOutput]; }",
        |_| {},
        Some(Arc::new(|_: &CodemodeHostCall| CodemodeOutcome::Err {
            message: "denied".into(),
            properties: Some(json!({ "structuredOutput": { "code": 7 }, "name": "Ignored", "message": "ignored" })),
        })),
    );
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": [true, "denied", "Error", { "code": 7 }] }));
}

async fn rejects_rejection_properties_that_are_not_an_object_and_tools_sharing_a_script_name(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = plain(&sandbox, "return await tools.wait(1)");
    wait_for(|| recorded.calls.lock().len() == 1).await;
    for properties in [Value::Null, json!([1])] {
        let error = recorded
            .handle
            .deliver(1, STARTED_AT, 1, CodemodeOutcome::Err { message: "x".into(), properties: Some(properties) })
            .unwrap_err();
        assert!(error.contains("not an object"), "{error}");
    }
    recorded.handle.deliver(1, STARTED_AT, 1, CodemodeOutcome::ok(json!(1))).unwrap();
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": 1 }));
    let mut options = CodemodeRunOptions::new(STARTED_AT, 1, |_, _| {});
    options.tools = vec![CodemodeDeclaredTool::new("a-b"), CodemodeDeclaredTool::new("a_b")];
    let error = sandbox.start("", options).err().unwrap();
    assert!(error.0.contains("script names must be unique"), "{error}");
}

async fn uses_the_script_names_the_run_passes(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = start(
        &sandbox,
        "return [typeof tools.renamed, Object.keys(tools), ALL_TOOLS.map((tool) => tool.name)]",
        |options| {
            options.tools = vec![CodemodeDeclaredTool { name: "my-tool".into(), js_name: Some("renamed".into()), description: None }]
        },
        None,
    );
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": ["function", ["renamed", "my-tool"], ["renamed"]] }));
}

async fn ends_with_an_error_the_script_cannot_catch_when_the_caller_aborts(backend: Backend) {
    let sandbox = create(backend, None);
    let controller = AbortController::new();
    let signal = controller.signal();
    let mut recorded = start(
        &sandbox,
        r#"try { await tools.wait(1); } catch { text("caught"); }
        text("after");"#,
        move |options| options.signal = Some(signal),
        None,
    );
    wait_for(|| recorded.calls.lock().len() == 1).await;
    controller.abort_with_reason("diverged");
    let result = recorded.result().await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "aborted", "message": "diverged" }, "output": [] }));
    assert!(recorded.handle.deliver(1, STARTED_AT, 1, CodemodeOutcome::ok(Value::Null)).is_ok());
}

async fn is_finished_once_it_has_its_outcome(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = plain(&sandbox, "await tools.wait(1); return 2");
    wait_for(|| recorded.calls.lock().len() == 1).await;
    assert!(!recorded.handle.finished());
    recorded.handle.deliver(1, STARTED_AT, 1, CodemodeOutcome::ok(Value::Null)).unwrap();
    recorded.result().await;
    assert!(recorded.handle.finished());
}

async fn rejects_a_call_that_is_not_waiting_and_a_result_number_used_twice(backend: Backend) {
    let sandbox = create(backend, None);
    let mut recorded = plain(&sandbox, "await Promise.all([tools.wait(1), tools.wait(2)])");
    wait_for(|| recorded.calls.lock().len() == 2).await;
    let run = recorded.handle.clone();
    assert!(run.deliver(1, STARTED_AT, 99, CodemodeOutcome::ok(Value::Null)).unwrap_err().contains("Call 99 is not waiting"));
    run.deliver(1, STARTED_AT, 1, CodemodeOutcome::ok(Value::Null)).unwrap();
    assert!(run.deliver(1, STARTED_AT, 2, CodemodeOutcome::ok(Value::Null)).unwrap_err().contains("used once"));
    assert!(run.deliver(0, STARTED_AT, 2, CodemodeOutcome::ok(Value::Null)).unwrap_err().contains("used once"));
    run.deliver(2, STARTED_AT, 2, CodemodeOutcome::ok(Value::Null)).unwrap();
    assert_matches(&recorded.result().await, json!({ "ok": true }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applies_results_in_seq_order_when_the_transport_reorders_replies_sent_in_order() {
    let sandbox = remote(None, true);
    let mut recorded = plain(
        &sandbox,
        r#"const order = [];
        await Promise.all([tools.wait("a").then(() => order.push("a")), tools.wait("b").then(() => order.push("b"))]);
        return order;"#,
    );
    wait_for(|| recorded.calls.lock().len() == 2).await;
    let calls = recorded.calls();
    recorded.handle.deliver(1, STARTED_AT + 1000.0, calls[0].id, CodemodeOutcome::ok(Value::Null)).unwrap();
    recorded.handle.deliver(2, STARTED_AT + 2000.0, calls[1].id, CodemodeOutcome::ok(Value::Null)).unwrap();
    assert_matches(&recorded.result().await, json!({ "ok": true, "value": ["a", "b"] }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execute_reads_the_real_clock_and_real_randomness() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions::default()).unwrap();
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64;
    let first = run(&sandbox, "return [Date.now(), Math.random()]").await;
    let second = run(&sandbox, "return [Date.now(), Math.random()]").await;
    let after = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64;
    let now = first.value().unwrap()[0].as_f64().unwrap();
    assert!(now >= before && now <= after);
    assert_ne!(first.value().unwrap()[1], second.value().unwrap()[1]);
}
