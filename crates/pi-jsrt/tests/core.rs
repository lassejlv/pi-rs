//! Core runtime behavior: eval, errors, host functions, handles, timers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use pi_jsrt::{Error, JsRuntime, RuntimeOptions};
use serde_json::{Value, json};

fn capture() -> (RuntimeOptions, Arc<Mutex<String>>) {
    let out = Arc::new(Mutex::new(String::new()));
    let o1 = out.clone();
    let o2 = out.clone();
    let opts = RuntimeOptions {
        stdout: Some(Arc::new(move |b: &[u8]| o1.lock().push_str(&String::from_utf8_lossy(b)))),
        stderr: Some(Arc::new(move |b: &[u8]| o2.lock().push_str(&String::from_utf8_lossy(b)))),
        ..Default::default()
    };
    (opts, out)
}

#[tokio::test]
async fn eval_returns_json() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    assert_eq!(rt.eval("1 + 2").await.unwrap(), json!(3));
    assert_eq!(rt.eval("({a: [1, 'x', null, true], b: undefined})").await.unwrap(), json!({"a": [1, "x", null, true]}));
    assert_eq!(rt.eval("Promise.resolve(41).then(x => x + 1)").await.unwrap(), json!(42));
    assert_eq!(rt.eval("0.5").await.unwrap(), json!(0.5));
    assert_eq!(rt.eval("new Map([['k', 1]])").await.unwrap(), json!({"k": 1}));
}

#[tokio::test]
async fn typescript_eval() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let v = rt
        .eval_script("enum E { A = 1, B } const x: number = E.B; interface I {}; x as number", "snippet.ts")
        .await
        .unwrap();
    assert_eq!(v, json!(2));
}

#[tokio::test]
async fn exceptions_map_to_errors() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let err = rt.eval("function boom() { throw new TypeError('bad thing'); }\nboom()").await.unwrap_err();
    let Error::Js(e) = err else { panic!("expected JS error") };
    assert_eq!(e.name, "TypeError");
    assert_eq!(e.message, "bad thing");
    let stack = e.stack.unwrap();
    assert!(stack.starts_with("TypeError: bad thing\n"), "{stack}");
    assert!(stack.contains("boom"), "{stack}");

    let err = rt.eval("Promise.reject(new RangeError('nope'))").await.unwrap_err();
    assert_eq!(err.as_js().unwrap().name, "RangeError");

    let err = rt.eval("throw 'plain string'").await.unwrap_err();
    assert_eq!(err.to_string(), "plain string");

    let err = rt.eval("let = ;").await.unwrap_err();
    assert_eq!(err.as_js().unwrap().name, "SyntaxError");
}

#[tokio::test]
async fn typescript_stack_is_source_mapped() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let src = "interface A {\n  x: number;\n}\ntype B = string;\n\nexport function f(a: A): never {\n  throw new Error('mapped ' + a.x);\n}\n";
    let ns = rt.eval_module(src, "mapped.ts").await.unwrap();
    let err = ns.call_method("f", vec![json!({"x": 1})]).await.unwrap_err();
    let stack = err.as_js().unwrap().stack.clone().unwrap();
    assert!(stack.contains("mapped.ts:7:"), "{stack}");
}

#[tokio::test]
async fn sync_and_async_host_functions() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    rt.register_function("host.add", |_rt, args| {
        let a = args[0].as_f64().unwrap_or(0.0);
        let b = args[1].as_f64().unwrap_or(0.0);
        Ok(json!(a + b))
    })
    .await
    .unwrap();
    rt.register_function("host.fail", |_rt, _args| Err(Error::host("host said no")))
        .await
        .unwrap();
    rt.register_async_function("host.later", |_rt, args| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(json!({ "echo": args[0] }))
    })
    .await
    .unwrap();
    rt.register_async_function("host.failLater", |_rt, _args| async move { Err(Error::js("TypeError", "async no")) })
        .await
        .unwrap();

    assert_eq!(rt.eval("host.add(2, 3)").await.unwrap(), json!(5));
    assert_eq!(
        rt.eval("try { host.fail() } catch (e) { e instanceof Error ? e.message : 'x' }").await.unwrap(),
        json!("host said no")
    );
    assert_eq!(rt.eval("host.later('hi').then(r => r.echo)").await.unwrap(), json!("hi"));
    assert_eq!(
        rt.eval("host.failLater().catch(e => e.name + ':' + e.message)").await.unwrap(),
        json!("TypeError:async no")
    );
}

#[tokio::test]
async fn callbacks_from_rust() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let stored = Arc::new(Mutex::new(Vec::new()));
    let s2 = stored.clone();
    rt.register_function("pi.on", move |rt, args| {
        let name = args[0].as_str().unwrap_or_default().to_string();
        let handle = rt.handle(&args[1]).expect("function marker");
        s2.lock().push((name, handle));
        Ok(Value::Null)
    })
    .await
    .unwrap();
    rt.eval(
        r#"
        let calls = 0;
        pi.on("tool_call", async (event) => { calls++; await null; return { seen: event.name, calls }; });
        pi.on("sync", (a, b) => a * b);
        "#,
    )
    .await
    .unwrap();
    let handlers = stored.lock().clone();
    assert_eq!(handlers.len(), 2);
    let (name, cb) = &handlers[0];
    assert_eq!(name, "tool_call");
    assert_eq!(cb.call(vec![json!({"name": "bash"})]).await.unwrap(), json!({"seen": "bash", "calls": 1}));
    assert_eq!(cb.call(vec![json!({"name": "read"})]).await.unwrap(), json!({"seen": "read", "calls": 2}));

    // Blocking call from a plain thread (e.g. a render loop).
    let sync_cb = handlers[1].1.clone();
    let r = std::thread::spawn(move || sync_cb.call_blocking(vec![json!(6), json!(7)])).join().unwrap();
    assert_eq!(r.unwrap(), json!(42));

    // Unadopted function markers are swept after the host call.
    rt.register_function("pi.ignore", |_rt, _args| Ok(Value::Null)).await.unwrap();
    let before = rt.handle_count().await.unwrap();
    rt.eval("pi.ignore(() => 1, { nested: () => 2 })").await.unwrap();
    assert_eq!(rt.handle_count().await.unwrap(), before);

    // Dropping a handle releases it.
    drop(handlers);
    stored.lock().clear();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(rt.handle_count().await.unwrap(), before - 2);
}

#[tokio::test]
async fn object_handles_and_methods() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let factory = rt
        .eval_handle(
            r#"(class Component {
                constructor(label) { this.label = label; this.renders = 0; }
                render(width) { this.renders++; return [this.label.padEnd(width, '.')]; }
                invalidate() { this.renders = 0; }
            })"#,
        )
        .await
        .unwrap();
    let comp = factory.construct(vec![json!("hi")]).await.unwrap();
    let c2 = comp.clone();
    let lines = std::thread::spawn(move || c2.call_method_blocking("render", vec![json!(5)])).join().unwrap().unwrap();
    assert_eq!(lines, json!(["hi..."]));
    assert_eq!(comp.get("renders").await.unwrap(), json!(1));
    comp.set("label", json!("yo")).await.unwrap();
    assert_eq!(comp.call_method("render", vec![json!(3)]).await.unwrap(), json!(["yo."]));
    assert_eq!(comp.type_of().await.unwrap(), "object");

    // Passing a handle back into JS.
    let check = rt.eval_handle("(c) => c.renders").await.unwrap();
    assert_eq!(check.call(vec![comp.to_value()]).await.unwrap(), json!(2));
}

#[tokio::test]
async fn host_function_values() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let done = rt
        .host_function(move |_rt, args| {
            c.fetch_add(args.len(), Ordering::SeqCst);
            Ok(json!("done"))
        })
        .await
        .unwrap();
    let f = rt.eval_handle("(done) => done(1, 2, 3) + '!'").await.unwrap();
    assert_eq!(f.call(vec![done]).await.unwrap(), json!("done!"));
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn timers_work() {
    let (opts, out) = capture();
    let rt = JsRuntime::new(opts).unwrap();
    let v = rt
        .eval(
            r#"
            new Promise((resolve) => {
                const order = [];
                // Resolve once the interval's third tick and t20 have both run, not on a fixed
                // deadline: a starved loop (parallel tests) would otherwise fire a 60 ms resolve
                // before the rescheduled interval ticks, in Node as here.
                let t20 = false;
                let n = 0;
                const done = () => { if (t20 && n === 3) setImmediate(() => resolve(order)); };
                setTimeout(() => { order.push('t20'); t20 = true; done(); }, 20);
                const t = setTimeout(() => order.push('cancelled'), 5);
                clearTimeout(t);
                setImmediate(() => order.push('immediate'));
                queueMicrotask(() => order.push('micro'));
                const iv = setInterval(() => { order.push('iv' + (++n)); if (n === 3) { clearInterval(iv); done(); } }, 2);
            })
            "#,
        )
        .await
        .unwrap();
    let order: Vec<String> = serde_json::from_value(v).unwrap();
    assert_eq!(order[0], "micro");
    assert!(order.contains(&"immediate".to_string()));
    assert!(order.contains(&"t20".to_string()));
    assert!(!order.contains(&"cancelled".to_string()));
    assert_eq!(order.iter().filter(|s| s.starts_with("iv")).count(), 3);
    assert_eq!(rt.timer_count().await.unwrap(), 0);
    // Uncaught errors in timers are reported, not fatal.
    rt.eval("setTimeout(() => { throw new Error('timer boom') }, 1); new Promise(r => setTimeout(r, 20))")
        .await
        .unwrap();
    assert!(out.lock().contains("timer boom"), "{}", out.lock());
}

#[tokio::test]
async fn console_and_inspect() {
    let (opts, out) = capture();
    let rt = JsRuntime::new(opts).unwrap();
    rt.eval(
        r#"
        console.log("hello %s, %d items", "world", 3, { a: 1, b: [1, 2], c: "x" });
        console.error(new Map([["k", { deep: { deeper: { deepest: 1 } } }]]));
        class Foo { constructor() { this.x = 1; } }
        console.log(new Foo(), [undefined, null], () => 1, Symbol("s"), 10n);
        "#,
    )
    .await
    .unwrap();
    let text = out.lock().clone();
    assert!(text.contains("hello world, 3 items { a: 1, b: [ 1, 2 ], c: 'x' }"), "{text}");
    assert!(text.contains("Map(1) { 'k' => { deep: { deeper: [Object] } } }"), "{text}");
    assert!(text.contains("Foo { x: 1 } [ undefined, null ] [Function (anonymous)] Symbol(s) 10n"), "{text}");
}

#[tokio::test]
async fn unhandled_rejection_reported() {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let m = messages.clone();
    let opts = RuntimeOptions {
        on_uncaught: Some(Arc::new(move |kind: &str, e: &Error| m.lock().push(format!("{kind}: {e}")))),
        ..Default::default()
    };
    let rt = JsRuntime::new(opts).unwrap();
    rt.eval("Promise.reject(new Error('lost')); const p = Promise.reject(new Error('handled')); p.catch(() => {}); 1")
        .await
        .unwrap();
    rt.with_context(|_| ()).await.unwrap();
    let msgs = messages.lock().clone();
    assert_eq!(msgs, vec!["Unhandled promise rejection: Error: lost".to_string()]);
}

#[tokio::test]
async fn web_globals() {
    let rt = JsRuntime::new(RuntimeOptions::sandboxed()).unwrap();
    let v = rt
        .eval(
            r#"
            const u = new URL("https://user:pw@example.com:8080/a/b?x=1&y=two#frag");
            u.searchParams.append("z", "a b&c");
            const enc = new TextEncoder().encode("héllo €");
            const dec = new TextDecoder().decode(enc);
            const ac = new AbortController();
            let aborted = null;
            ac.signal.addEventListener("abort", () => { aborted = ac.signal.reason.name; });
            ac.abort();
            const clone = structuredClone({ d: new Date(0), m: new Map([[1, { s: new Set([2]) }]]) });
            ({
                host: u.host, path: u.pathname, search: u.search, x: u.searchParams.get("x"),
                rel: new URL("../c?q", "https://e.com/a/b/").href,
                bytes: enc.length, dec, b64: btoa("hello"), unb64: atob("aGVsbG8="),
                aborted, cloneDate: clone.d instanceof Date, cloneSet: clone.m.get(1).s.has(2),
                usp: new URLSearchParams({ a: "1", b: "x y" }).toString(),
                uuid: crypto.randomUUID().length,
                hasProcess: typeof process, hasRequire: typeof require,
                num: (1234567.891).toLocaleString(),
                seg: [...new Intl.Segmenter().segment("é👍🏽x")].length,
            })
            "#,
        )
        .await
        .unwrap();
    assert_eq!(
        v,
        json!({
            "host": "example.com:8080", "path": "/a/b", "search": "?x=1&y=two&z=a+b%26c", "x": "1",
            "rel": "https://e.com/a/c?q",
            "bytes": 10, "dec": "héllo €", "b64": "aGVsbG8=", "unb64": "hello",
            "aborted": "AbortError", "cloneDate": true, "cloneSet": true,
            "usp": "a=1&b=x+y", "uuid": 36, "hasProcess": "undefined", "hasRequire": "undefined",
            "num": "1,234,567.891", "seg": 3
        })
    );
}

#[tokio::test]
async fn with_context_escape_hatch() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let n = rt
        .with_context(|ctx| {
            let v = ctx.eval(pi_jsrt::boa::Source::from_bytes("[1,2,3].length")).unwrap();
            v.as_number().unwrap()
        })
        .await
        .unwrap();
    assert_eq!(n, 3.0);
    let h = rt
        .with_context(|ctx| {
            let v = ctx.eval(pi_jsrt::boa::Source::from_bytes("({ greet: (n) => 'hi ' + n })")).unwrap();
            pi_jsrt::ctx::make_handle(ctx, v).unwrap()
        })
        .await
        .unwrap();
    assert_eq!(h.call_method("greet", vec![json!("bob")]).await.unwrap(), json!("hi bob"));
}

#[test]
fn runtime_without_tokio_context() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    rt.with_context_blocking(|_| ()).unwrap();
    let f = futures::executor::block_on(rt.eval_handle("(x) => new Promise(r => setTimeout(() => r(x * 2), 5))")).unwrap();
    assert_eq!(f.call_blocking(vec![json!(4)]).unwrap(), json!(8));
}

/// A blocking call made by a synchronous host function on the JS thread runs re-entrantly
/// (instead of deadlocking on the queue behind the host call itself).
#[tokio::test]
async fn blocking_call_from_host_function_runs_reentrantly() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let cb = rt.eval_handle("() => 1").await.unwrap();
    let cb2 = cb.clone();
    rt.register_function("host.reenter", move |_rt, _| match cb2.call_blocking(vec![]) {
        Err(Error::Deadlock) => Ok(json!("deadlock")),
        other => Ok(json!(format!("{other:?}"))),
    })
    .await
    .unwrap();
    assert_eq!(rt.eval("host.reenter()").await.unwrap(), json!("Ok(Number(1))"));
}

#[tokio::test]
async fn runaway_scripts_are_contained() {
    let rt = JsRuntime::new(RuntimeOptions {
        loop_iteration_limit: Some(1_000_000),
        ..RuntimeOptions::sandboxed()
    })
    .unwrap();
    let err = rt.eval("let i = 0; while (true) { i++; }").await.unwrap_err();
    assert!(err.to_string().to_lowercase().contains("loop"), "{err}");
    let err = rt.eval("function f(n) { return f(n + 1) + 1; } f(0)").await.unwrap_err();
    // Node's stack overflow error (vendor/boa_engine/PATCH.md: catchable, as in V8).
    assert!(err.to_string().contains("Maximum call stack size exceeded"), "{err}");
    // The runtime is still usable afterwards.
    assert_eq!(rt.eval("1 + 1").await.unwrap(), json!(2));
}

#[tokio::test]
async fn object_virtual_module() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    rt.register_function("host.version", |_rt, _| Ok(json!("1.2.3"))).await.unwrap();
    let obj = rt
        .eval_handle("({ getVersion: () => host.version(), CONST: 7 })")
        .await
        .unwrap();
    rt.set_virtual_module("@host/api", pi_jsrt::VirtualModule::Object(obj));
    let ns = rt
        .eval_module("import api, { getVersion, CONST } from '@host/api'; export default [getVersion(), CONST, api.CONST];", "x.mjs")
        .await
        .unwrap();
    assert_eq!(ns.get("default").await.unwrap(), json!(["1.2.3", 7, 7]));
}

/// Concurrency regression: a sync host function that waits for another thread which calls back
/// into JS (blocking) must not deadlock — the JS thread services the call while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_function_waiting_on_a_thread_that_calls_back_into_js() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    rt.register_function("host.waitForWorker", |rt, args| {
        let callback = rt.handle(&args[0]).expect("function marker");
        let value = rt.wait_servicing(move |done| {
            std::thread::spawn(move || {
                // Blocking call into JS from the thread the JS thread is waiting on.
                let doubled = callback.call_blocking(vec![json!(21)]);
                done(doubled);
            });
        });
        match value {
            Some(result) => result,
            None => Err(Error::host("worker dropped the completion")),
        }
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        rt.eval("let seen = []; const r = host.waitForWorker((x) => { seen.push(x); return x * 2; }); [r, seen.length]"),
    )
    .await
    .expect("host function waiting on a thread that calls into JS deadlocked")
    .unwrap();
    assert_eq!(result, json!([42, 1]));
}

/// `new FinalizationRegistry()` queues a cleanup job that waits for a GC notification; it must
/// not keep microtask draining (and therefore every eval and module import) from finishing.
#[tokio::test]
async fn finalization_registry_does_not_block_the_job_queue() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let v = tokio::time::timeout(
        Duration::from_secs(10),
        rt.eval("const r = new FinalizationRegistry(() => {}); r.register({}, 1); Promise.resolve(1).then((x) => x + 1)"),
    )
    .await
    .expect("job queue drained")
    .unwrap();
    assert_eq!(v, json!(2));
    let ns = tokio::time::timeout(
        Duration::from_secs(10),
        rt.eval_module("const r = new FinalizationRegistry(() => {}); await null; export const ok = typeof r.register;", "fr.mjs"),
    )
    .await
    .expect("module evaluated")
    .unwrap();
    assert_eq!(ns.get("ok").await.unwrap(), json!("function"));
}

/// Concurrency regression: another thread's blocking call into JS that was queued *before* the JS
/// thread started waiting on that thread (in a sync host function) must still be serviced. The
/// extension bridge hit this when a handler queued `addAutocompleteProvider` (the UI thread reads
/// the provider's trigger characters from JS) and then blocked on a UI getter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_call_queued_before_the_js_thread_starts_servicing() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let slot: Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<pi_jsrt::Result<serde_json::Value>>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let s1 = slot.clone();
    rt.register_function("host.queueCall", move |rt, args| {
        let callback = rt.handle(&args[0]).expect("function marker");
        let (tx, rx) = std::sync::mpsc::channel();
        *s1.lock().unwrap() = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(callback.call_blocking(vec![json!(21)]));
        });
        // Let the worker's call reach the JS thread's queue while this host call still runs.
        std::thread::sleep(Duration::from_millis(100));
        Ok(json!(null))
    })
    .await
    .unwrap();
    let s2 = slot.clone();
    rt.register_function("host.waitForQueued", move |rt, _args| {
        let rx = s2.lock().unwrap().take().expect("queueCall first");
        let value = rt.wait_servicing(move |done| {
            std::thread::spawn(move || {
                done(rx.recv().unwrap_or(Err(Error::host("worker gone"))));
            });
        });
        value.unwrap_or_else(|| Err(Error::host("worker dropped the completion")))
    })
    .await
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let rt2 = rt.clone();
    tokio::spawn(async move {
        let _ = tx.send(rt2.eval("host.queueCall((x) => x * 2); host.waitForQueued()").await);
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => assert_eq!(result.unwrap(), json!(42)),
        Err(_) => {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "deadlock: queued blocking call was never serviced");
            std::process::exit(101);
        }
    }
}

/// The embedder's blocking-wait hook wraps waits of other threads for the JS thread (pi releases
/// its UI lock there).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_wait_hook_wraps_waits_for_the_js_thread() {
    static WAITS: AtomicUsize = AtomicUsize::new(0);
    pi_jsrt::set_blocking_wait_hook(|wait| {
        WAITS.fetch_add(1, Ordering::SeqCst);
        wait();
    });
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let f = rt.eval_handle("(x) => x + 1").await.unwrap();
    let before = WAITS.load(Ordering::SeqCst);
    let result = tokio::task::spawn_blocking(move || f.call_blocking(vec![json!(1)])).await.unwrap().unwrap();
    assert_eq!(result, json!(2));
    assert!(WAITS.load(Ordering::SeqCst) > before);
}
