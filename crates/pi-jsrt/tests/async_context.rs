//! AsyncLocalStorage propagation (promises/await, timers, immediates, nextTick, microtasks,
//! AsyncResource, bind/snapshot, enterWith/exit/disable), async_hooks, and Node's nextTick vs
//! promise job ordering, diagnostics_channel. Expected outputs are Node 22's (scripts run by `pi-jsrt-node`).

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

fn run(name: &str, script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, script).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn async_local_storage_follows_async_continuations() {
    let actual: Vec<serde_json::Value> = serde_json::from_str(&run("als.js", ALS)).unwrap();
    let expected: Vec<serde_json::Value> = serde_json::from_str(ALS_NODE).unwrap();
    assert_eq!(actual.len(), expected.len());
    let start = expected
        .iter()
        .position(|row| row[0] == "sync after run")
        .unwrap()
        + 1;
    let end = expected
        .iter()
        .position(|row| row[0] == "resource")
        .unwrap();
    assert_eq!(actual[..start], expected[..start]);
    assert_eq!(actual[end..], expected[end..]);
    // These independent 5 ms timers can land on different loop iterations.
    // Node itself interleaves A's immediate/timeout with B's continuation.
    // Compare every event and store value in each task's causal order instead.
    for tag in ["A", "B"] {
        let actual = actual[start..end]
            .iter()
            .filter(|row| row[0] == tag)
            .collect::<Vec<_>>();
        let expected = expected[start..end]
            .iter()
            .filter(|row| row[0] == tag)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "async context {tag}");
    }
}

#[test]
fn diagnostics_channel_publish_stores_tracing_and_builtin_channels() {
    assert_eq!(run("dc.js", DC), DC_NODE);
}

#[test]
fn next_tick_runs_before_promise_jobs() {
    assert_eq!(run("order.js", ORDER), ORDER_NODE);
}

const ALS: &str = r###"const { AsyncLocalStorage, AsyncResource, executionAsyncId, triggerAsyncId, createHook } = require("async_hooks");
const als = new AsyncLocalStorage();
const other = new AsyncLocalStorage();
const log = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function work(tag) {
  log.push([tag, "start", als.getStore()]);
  await sleep(5);
  log.push([tag, "after await", als.getStore()]);
  await Promise.resolve();
  setTimeout(() => log.push([tag, "timeout", als.getStore()]), 1);
  setImmediate(() => log.push([tag, "immediate", als.getStore()]));
  process.nextTick(() => log.push([tag, "nextTick", als.getStore()]));
  queueMicrotask(() => log.push([tag, "microtask", als.getStore()]));
  other.run("inner", () => log.push([tag, "nested", als.getStore(), other.getStore()]));
  log.push([tag, "after nested", other.getStore()]);
}
log.push(["outside", als.getStore()]);
const p1 = als.run({ id: 1 }, () => work("A"));
const p2 = als.run({ id: 2 }, () => work("B"));
log.push(["sync after run", als.getStore()]);
const r = als.run(7, () => new AsyncResource("X"));
const bound = als.run(8, () => AsyncLocalStorage.bind(() => als.getStore()));
const snap = als.run(9, () => AsyncLocalStorage.snapshot());
Promise.all([p1, p2]).then(async () => {
  await sleep(10);
  log.push(["resource", r.runInAsyncScope(() => als.getStore()), typeof r.asyncId(), r.asyncId() > 1]);
  log.push(["bound", bound(), "snapshot", snap(() => als.getStore())]);
  als.enterWith("entered");
  log.push(["enterWith", als.getStore()]);
  await null;
  log.push(["enterWith after await", als.getStore()]);
  log.push(["exit", als.exit(() => als.getStore()), als.getStore()]);
  als.disable();
  log.push(["disabled", als.getStore()]);
  const ids = [typeof executionAsyncId(), typeof triggerAsyncId()];
  const seen = [];
  const hook = createHook({ init(id, type) { if (type === "Timeout") seen.push("init " + type); }, before() {}, after() {}, destroy() {} }).enable();
  await new Promise((res) => setTimeout(res, 1));
  hook.disable();
  log.push(["hooks", ids, seen]);
  try { createHook({ init: 1 }); } catch (e) { log.push([e.code, e.message]); }
  console.log(JSON.stringify(log));
});
"###;
const ALS_NODE: &str = r###"[["outside",null],["A","start",{"id":1}],["B","start",{"id":2}],["sync after run",null],["A","after await",{"id":1}],["A","nested",{"id":1},"inner"],["A","after nested",null],["A","microtask",{"id":1}],["A","nextTick",{"id":1}],["B","after await",{"id":2}],["B","nested",{"id":2},"inner"],["B","after nested",null],["B","microtask",{"id":2}],["B","nextTick",{"id":2}],["A","immediate",{"id":1}],["B","immediate",{"id":2}],["A","timeout",{"id":1}],["B","timeout",{"id":2}],["resource",7,"number",true],["bound",8,"snapshot",9],["enterWith","entered"],["enterWith after await","entered"],["exit",null,"entered"],["disabled",null],["hooks",["number","number"],["init Timeout"]],["ERR_ASYNC_CALLBACK","hook.init must be a function"]]
"###;
const ORDER: &str = r###"const out = [];
setTimeout(() => out.push("timeout"), 0);
Promise.resolve().then(() => { out.push("promise1"); process.nextTick(() => out.push("tick-in-promise")); }).then(() => out.push("promise2"));
process.nextTick(() => { out.push("tick1"); Promise.resolve().then(() => out.push("promise-in-tick")); process.nextTick(() => out.push("tick-in-tick")); });
queueMicrotask(() => out.push("microtask"));
process.nextTick(() => out.push("tick2"));
setTimeout(() => { out.push("t2"); process.nextTick(() => out.push("t2 tick")); Promise.resolve().then(() => out.push("t2 promise")); }, 5);
setTimeout(() => { out.push("t3"); }, 5);
setTimeout(() => console.log(out.join(",")), 30);
"###;
const ORDER_NODE: &str = r###"tick1,tick2,tick-in-tick,promise1,microtask,promise-in-tick,promise2,tick-in-promise,timeout,t2,t2 tick,t2 promise,t3
"###;
const DC: &str = r###"const dc = require("diagnostics_channel");
const { AsyncLocalStorage } = require("async_hooks");
const http = require("http");
const log = [];
const ch = dc.channel("my-channel");
log.push(["initial", ch.hasSubscribers, dc.hasSubscribers("my-channel"), ch instanceof dc.Channel, ch === dc.channel("my-channel"), ch.name]);
const onMsg = (msg, name) => log.push(["got", msg, name]);
dc.subscribe("my-channel", onMsg);
log.push(["after subscribe", ch.hasSubscribers]);
ch.publish({ a: 1 });
log.push(["unsubscribe", dc.unsubscribe("my-channel", onMsg), dc.unsubscribe("my-channel", onMsg), ch.hasSubscribers]);
ch.publish({ ignored: true });
const store = new AsyncLocalStorage();
const ch2 = dc.channel("store-channel");
ch2.bindStore(store, (data) => ({ wrapped: data }));
log.push(["runStores", ch2.runStores({ x: 1 }, function (a) { return [store.getStore(), this.t, a]; }, { t: "this" }, "arg"), store.getStore()]);
const tc = dc.tracingChannel("op");
const events = [];
tc.subscribe({ start: (c) => events.push(["start", c.n]), end: (c) => events.push(["end", c.result]), asyncStart: (c) => events.push(["asyncStart", c.result]), asyncEnd: () => events.push(["asyncEnd"]), error: (c) => events.push(["error", c.error.message]) });
log.push(["traceSync", tc.traceSync((x) => x * 2, { n: 1 }, null, 21)]);
try { tc.traceSync(() => { throw new Error("boom"); }, { n: 2 }); } catch (e) { log.push(["sync threw", e.message]); }
try { dc.channel(5); } catch (e) { log.push([e.code, e.message]); }
dc.subscribe("http.client.request.start", ({ request }) => log.push(["http.client.request.start", request.method, request.path]));
dc.subscribe("http.server.request.start", ({ request, response, server }) => log.push(["http.server.request.start", request.url, typeof response.end, typeof server.listen]));
dc.subscribe("http.client.response.finish", ({ response }) => log.push(["http.client.response.finish", response.statusCode]));
dc.subscribe("net.client.socket", ({ socket }) => log.push(["net.client.socket", typeof socket.write]));
(async () => {
  log.push(["tracePromise", await tc.tracePromise(async (x) => x + 1, { n: 3 }, null, 1)]);
  await new Promise((resolve) => tc.traceCallback((a, cb) => setTimeout(() => cb(null, a + "!"), 1), -1, { n: 4 }, null, "cb", (err, r) => { log.push(["traceCallback", err, r]); resolve(); }));
  const server = http.createServer((req, res) => res.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  await new Promise((r) => http.get(`http://127.0.0.1:${server.address().port}/dc`, (res) => { res.resume(); res.on("end", r); }));
  server.close();
  log.push(["events", events]);
  console.log(JSON.stringify(log));
})();
"###;
const DC_NODE: &str = r###"[["initial",false,false,true,true,"my-channel"],["after subscribe",true],["got",{"a":1},"my-channel"],["unsubscribe",true,false,false],["runStores",[{"wrapped":{"x":1}},"this","arg"],null],["traceSync",42],["sync threw","boom"],["ERR_INVALID_ARG_TYPE","The \"channel\" argument must be one of type string or symbol. Received type number (5)"],["tracePromise",2],["traceCallback",null,"cb!"],["net.client.socket","function"],["http.client.request.start","GET","/dc"],["http.server.request.start","/dc","function","function"],["http.client.response.finish",200],["events",[["start",1],["end",42],["start",2],["error","boom"],["end",null],["start",3],["end",null],["asyncStart",2],["asyncEnd"],["start",4],["end",null],["asyncStart","cb!"],["asyncEnd"]]]]
"###;
