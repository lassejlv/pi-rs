//! Port of `packages/codemode/test/portable.test.ts`. The TS `InlineCodemodeSandbox` (a VM on the
//! calling thread with an interrupt budget) maps to the thread sandbox with `interrupt_budget`.

mod common;

use std::sync::Arc;

use common::*;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_codemode::*;
use serde_json::json;
use tokio::task::JoinHandle;

/// An in-process remote that records what crosses the boundary, round-tripped through JSON.
#[derive(Clone, Default)]
struct RecordingRemote {
    options: CodemodeServeOptions,
    exchanges: Arc<Mutex<Vec<usize>>>,
    served: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl CodemodeRemote for RecordingRemote {
    fn execute(&self, request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        let exchanges = self.exchanges.clone();
        let wrapped: CodemodeRemoteExchange = Arc::new(move |messages: Vec<CodemodeRemoteMessage>| {
            exchanges.lock().push(messages.len());
            let reply = exchange(roundtrip(&messages));
            Box::pin(async move { reply.await.map(|reply| roundtrip(&reply)) })
        });
        let task = tokio::spawn(serve_codemode_remote(roundtrip(&request), wrapped, self.options.clone()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = task.await;
            let _ = done_tx.send(());
        });
        self.served.lock().push(waiter);
        Box::pin(async move {
            let _ = done_rx.await;
            Ok(())
        })
    }
}

impl RecordingRemote {
    fn factory(&self) -> CodemodeRemoteFactory {
        let remote = self.clone();
        Arc::new(move || Ok(Arc::new(remote.clone()) as Arc<dyn CodemodeRemote>))
    }

    async fn all_served(&self) {
        let served: Vec<JoinHandle<()>> = self.served.lock().drain(..).collect();
        for task in served {
            task.await.unwrap();
        }
    }
}

fn remote_sandbox(remote: &RecordingRemote, tools: Vec<CodemodeTool>) -> CodemodeSandbox {
    let mut options = RemoteCodemodeSandboxOptions::new(|| Err("unused".to_string()));
    options.remote = remote.factory();
    options.tools = tools;
    CodemodeSandbox::remote(options).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fails_a_script_that_computes_past_its_interrupt_budget_as_a_timeout() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions { interrupt_budget: Some(1_000), ..Default::default() }).unwrap();
    for code in ["while (true) {}", "while (true) await null"] {
        let result = run_with(&sandbox, code, CodemodeExecuteOptions { timeout_ms: Some(f64::INFINITY), ..Default::default() }).await;
        assert_matches(
            &result,
            json!({ "ok": false, "error": { "kind": "timeout", "message": "Execution exceeded its interrupt budget of 1000" } }),
        );
    }
}

/// QuickJS polls its interrupt handler every few thousand opcodes; here every loop iteration and
/// promise job counts, so the inner loop is shorter than in the TS test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn counts_the_budget_across_waits_on_tools() {
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
        interrupt_budget: Some(1_000),
        tools: vec![CodemodeTool::new_sync("tick", |_, _| Ok(Some(json!(1))))],
        ..Default::default()
    })
    .unwrap();
    let result = run(&sandbox, "for (;;) { for (let i = 0; i < 100; i++); await tools.tick(); }").await;
    assert_matches(&result, json!({ "ok": false, "error": { "kind": "timeout" } }));
    assert!(result.calls.len() > 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_tools_in_the_caller_and_the_script_on_the_remote() {
    let remote = RecordingRemote::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let sandbox = remote_sandbox(
        &remote,
        vec![CodemodeTool::new_sync("add", move |args, _| {
            let args = args.unwrap();
            recorder.lock().push(args.clone());
            Ok(Some(json!(args["a"].as_f64().unwrap() + 1.0)))
        })],
    );
    let result = run_with(
        &sandbox,
        "text('hi'); return await tools.add({ a: 1 })",
        CodemodeExecuteOptions { store: store(json!({ "k": 1 })), ..Default::default() },
    )
    .await;
    assert_matches(&result, json!({ "ok": true, "value": 2, "output": [{ "type": "text", "text": "hi" }] }));
    assert_eq!(*seen.lock(), vec![json!({ "a": 1 })]);
    assert_matches(&result, json!({ "calls": [{ "name": "add", "status": "ok" }] }));
    remote.all_served().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batches_output_instead_of_making_one_exchange_per_line() {
    let remote = RecordingRemote::default();
    let sandbox = remote_sandbox(&remote, vec![]);
    let result = run(&sandbox, "for (let i = 0; i < 5000; i++) console.log(i); return 'ok'").await;
    assert_matches(&result, json!({ "ok": true, "value": "ok" }));
    assert_eq!(result.output.len(), 5000);
    assert_eq!(result.output[4999], CodemodeOutputItem::Text { text: "4999".into(), console: Some(true) });
    assert!(remote.exchanges.lock().len() <= 6, "{:?}", remote.exchanges.lock());
}

/// The flush threshold counts characters as JavaScript does (UTF-16 code units), not UTF-8 bytes:
/// two 600k-character lines of a two-byte character stay under 1 MiB characters until the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn output_flush_threshold_counts_utf16_characters() {
    let remote = RecordingRemote::default();
    let sandbox = remote_sandbox(&remote, vec![]);
    let result = run(&sandbox, "const line = '\\u00e9'.repeat(600000); text(line); text(line); return 'ok'").await;
    assert_matches(&result, json!({ "ok": true, "value": "ok" }));
    assert_eq!(result.output.len(), 2);
    // One flush carrying both lines, then the terminal batch.
    assert_eq!(*remote.exchanges.lock(), vec![2, 1], "{:?}", remote.exchanges.lock());
    remote.all_served().await;
}

/// The memory cap is QuickJS's heap limit: an allocation past it throws QuickJS's
/// `InternalError: out of memory`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caps_the_interrupt_budget_and_memory_at_the_remotes_limits() {
    let remote = RecordingRemote {
        options: CodemodeServeOptions {
            max_interrupt_budget: Some(1_000),
            max_memory_limit_bytes: Some(8 * 1024 * 1024),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut options = RemoteCodemodeSandboxOptions::new(|| Err("unused".to_string()));
    options.remote = remote.factory();
    options.interrupt_budget = Some(1_000_000_000);
    let sandbox = CodemodeSandbox::remote(options).unwrap();
    assert_matches(
        &run(&sandbox, "while (true) {}").await,
        json!({ "ok": false, "error": { "kind": "timeout", "message": "Execution exceeded its interrupt budget of 1000" } }),
    );
    let memory = run(&sandbox, "try { new ArrayBuffer(16 * 1024 * 1024); return 'allocated'; } catch (error) { return error.name; }").await;
    assert_matches(&memory, json!({ "ok": true, "value": "InternalError" }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tells_the_remote_to_stop_when_the_caller_aborts() {
    let remote = RecordingRemote::default();
    let called = Arc::new(tokio::sync::Notify::new());
    let notify = called.clone();
    let sandbox = remote_sandbox(
        &remote,
        vec![CodemodeTool::new("hang", move |_, _| {
            notify.notify_one();
            std::future::pending()
        })],
    );
    let controller = AbortController::new();
    let signal = controller.signal();
    let pending = {
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            run_with(&sandbox, "await tools.hang(); return 'never'", CodemodeExecuteOptions { signal: Some(signal), ..Default::default() })
                .await
        })
    };
    called.notified().await;
    controller.abort();
    assert_matches(&pending.await.unwrap(), json!({ "ok": false, "error": { "kind": "aborted" } }));
    // The remote's pending exchange is answered with "stop", so its VM is stopped and execute() returns.
    tokio::time::timeout(std::time::Duration::from_secs(5), remote.all_served()).await.expect("served");
}

struct Failing;

impl CodemodeRemote for Failing {
    fn execute(&self, _request: CodemodeRemoteRequest, _exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("object reset".to_string()) })
    }
}

struct Silent;

impl CodemodeRemote for Silent {
    fn execute(&self, _request: CodemodeRemoteRequest, _exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_a_failing_or_vanishing_remote_as_a_sandbox_error() {
    let failing = CodemodeSandbox::remote(RemoteCodemodeSandboxOptions::new(|| Ok(Arc::new(Failing) as Arc<dyn CodemodeRemote>))).unwrap();
    assert_matches(
        &run(&failing, "return 1").await,
        json!({ "ok": false, "error": { "kind": "sandbox", "message": "Remote sandbox failed: object reset" } }),
    );

    let silent = CodemodeSandbox::remote(RemoteCodemodeSandboxOptions::new(|| Ok(Arc::new(Silent) as Arc<dyn CodemodeRemote>))).unwrap();
    assert_matches(
        &run(&silent, "return 1").await,
        json!({ "ok": false, "error": { "kind": "sandbox", "message": "Remote sandbox returned before the script settled" } }),
    );

    let unreachable = CodemodeSandbox::remote(RemoteCodemodeSandboxOptions::new(|| Err("no binding".to_string()))).unwrap();
    assert_matches(
        &run(&unreachable, "return 1").await,
        json!({ "ok": false, "error": { "kind": "sandbox", "message": "Failed to reach the remote sandbox: no binding" } }),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_with_the_in_process_remote_server() {
    let mut options =
        RemoteCodemodeSandboxOptions::new(|| Ok(Arc::new(CodemodeRemoteServer::default()) as Arc<dyn CodemodeRemote>));
    options.tools = vec![echo()];
    let sandbox = CodemodeSandbox::remote(options).unwrap();
    assert_matches(&run(&sandbox, "return await tools.echo([1, 'a'])").await, json!({ "ok": true, "value": [1, "a"] }));
}
