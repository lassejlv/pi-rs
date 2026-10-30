//! The embedded substrate: lossless value conversion, handles, typed errors, re-entrant calls
//! and the vitest shim.

use std::sync::Arc;

use pi_durable_jsrt::{
    Args, Call, Dispatch, ErrorClass, HostError, HostOptions, HostValue, Methods, call_async,
    call_sync, install,
};
use pi_json::strict::{self, JsonString};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

struct TestDispatch;

fn fail_with(args: &Args) -> HostError {
    let class =
        ErrorClass::from_name(&args.text(0).unwrap_or_default()).unwrap_or(ErrorClass::Error);
    let mut error = HostError::typed(class, args.string(1).cloned().unwrap_or_default());
    if let HostValue::Object(props) = args.get(2) {
        for (k, v) in props {
            error = error.with_prop(&k.to_string_lossy(), v.clone());
        }
    }
    if args.get(3).as_bool() == Some(true) {
        error = error.with_cause(HostError::new(
            ErrorClass::StorageRequestError,
            "root cause",
        ));
    }
    error
}

impl Dispatch for TestDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("test.echo", |_, args| Ok(HostValue::Array(args.0)));
        m.asynchronous("test.echoAsync", |_, args| async move {
            Ok(HostValue::Array(args.0))
        });
        m.sync("test.strict", |_, args| {
            // JS → strict JSON → JS without serde_json strings.
            let value = args.get(0).to_strict_omitting_undefined()?;
            Ok(HostValue::from_strict(&value))
        });
        m.sync("test.units", |_, args| {
            Ok(HostValue::Array(
                args.string(0)?
                    .units()
                    .iter()
                    .map(|u| HostValue::Number(f64::from(*u)))
                    .collect(),
            ))
        });
        m.sync("test.fail", |_, args| Err(fail_with(&args)));
        m.asynchronous(
            "test.failAsync",
            |_, args| async move { Err(fail_with(&args)) },
        );
        m.sync("test.counter", |_, args| {
            let start = args.f64(0)?;
            Ok(HostValue::native(parking_lot::Mutex::new(start)))
        });
        m.sync("test.increment", |_, args| {
            let counter = args.native::<parking_lot::Mutex<f64>>(0)?;
            let mut n = counter.lock();
            *n += 1.0;
            Ok(HostValue::Number(*n))
        });
        // Re-entrant: call a JS callback synchronously from inside a sync method.
        m.sync("test.callback", |_, mut args| {
            let f = args.handle(0)?;
            let rest = args.take(1);
            call_sync(&f, None, vec![rest])
        });
        m.sync("test.method", |_, args| {
            let obj = args.handle(0)?;
            call_sync(&obj, Some(&args.text(1)?), vec![args.get(2).clone()])
        });
        m.asynchronous("test.callbackAsync", |_: Call, mut args| async move {
            let f = args.handle(0)?;
            let rest = args.take(1);
            call_async(&f, None, vec![rest]).await
        });
        // A blocking call into JS from a tokio thread.
        m.asynchronous("test.callbackBlocking", |_: Call, mut args| async move {
            let f = args.handle(0)?;
            let rest = args.take(1);
            tokio::task::spawn_blocking(move || call_sync(&f, None, vec![rest]))
                .await
                .map_err(|e| HostError::type_error(e.to_string()))?
        });
    }
}

async fn runtime() -> (JsRuntime, pi_durable_jsrt::DurableHost) {
    let rt = JsRuntime::new(RuntimeOptions {
        timer_control: true,
        expose_gc: true,
        ..Default::default()
    })
    .unwrap();
    let mut dispatch = pi_durable_jsrt::dispatch::defaults();
    dispatch.push(Arc::new(TestDispatch));
    let host = install(
        &rt,
        HostOptions {
            vitest: true,
            dispatch,
        },
    )
    .await
    .unwrap();
    (rt, host)
}

async fn run(rt: &JsRuntime, src: &str) -> serde_json::Value {
    let ns = rt.eval_module(src, "/substrate-test.mjs").await.unwrap();
    ns.call_method("run", vec![]).await.unwrap()
}

#[tokio::test]
async fn values_round_trip_losslessly() {
    let (rt, host) = runtime().await;
    let out = run(
        &rt,
        r#"
        import { call, callAsync } from "pi-durable:bridge";
        const lone = ["\ud800", "a\udc00b", "􏿿", "x\ud83d", ""];
        const sample = { lone, undef: undefined, neg: -0, nan: NaN, inf: -Infinity, nested: [1, [2, { a: null }]], ["__proto__"]: 5, 10: "ten", b: true };
        export async function run() {
            const [back] = call("test.echo", sample);
            const [asyncBack] = await callAsync("test.echoAsync", sample);
            const check = (v) => ({
                lone: v.lone.every((s, i) => s === lone[i]),
                hasUndef: Object.prototype.hasOwnProperty.call(v, "undef") && v.undef === undefined,
                negZero: Object.is(v.neg, -0),
                nan: Number.isNaN(v.nan),
                inf: v.inf === -Infinity,
                nested: JSON.stringify(v.nested),
                proto: Object.getPrototypeOf(v) === Object.prototype && Object.prototype.hasOwnProperty.call(v, "__proto__") && v.__proto__ === 5,
                keys: Object.keys(v),
            });
            const units = call("test.units", "\ud800x\udfff");
            const strict = call("test.strict", { s: "\udc00", u: undefined, arr: [undefined, 1] });
            const strictOk = strict.s === "\udc00" && !("u" in strict) && strict.arr[0] === null;
            let bigint, cyclic, undefinedJson;
            try { call("test.echo", 1n); } catch (e) { bigint = e instanceof TypeError; }
            const loop = {}; loop.self = loop;
            try { call("test.echo", loop); } catch (e) { cyclic = e instanceof TypeError && /cyclic/.test(e.message); }
            try { call("test.strict", undefined); } catch (e) { undefinedJson = e instanceof TypeError; }
            const bytes = call("test.echo", new Uint8Array([0, 255, 7]))[0];
            const sparse = call("test.echo", [1, , 3])[0];
            return { sync: check(back), async: check(asyncBack), units, strictOk, bigint, cyclic, undefinedJson,
                bytes: [bytes instanceof Uint8Array, Array.from(bytes)], sparse: [sparse.length, 1 in sparse, sparse[1] === undefined] };
        }
        "#,
    )
    .await;
    let expected_check = json!({
        "lone": true, "hasUndef": true, "negZero": true, "nan": true, "inf": true,
        "nested": "[1,[2,{\"a\":null}]]", "proto": true,
        "keys": ["10", "lone", "undef", "neg", "nan", "inf", "nested", "__proto__", "b"],
    });
    assert_eq!(out["sync"], expected_check);
    assert_eq!(out["async"], expected_check);
    assert_eq!(out["units"], json!([0xd800, 0x78, 0xdfff]));
    assert_eq!(out["strictOk"], json!(true));
    assert_eq!(out["bigint"], json!(true));
    assert_eq!(out["cyclic"], json!(true));
    assert_eq!(out["undefinedJson"], json!(true));
    assert_eq!(out["bytes"], json!([true, [0, 255, 7]]));
    assert_eq!(out["sparse"], json!([3, true, true]));
    assert!(host.counters()["test.echo"] >= 4);
}

#[test]
fn strict_values_keep_utf16_units() {
    let lone = JsonString::from_utf16(vec![0x61, 0xd800, 0x62]);
    let value = HostValue::object([
        ("s", HostValue::String(lone.clone())),
        ("n", HostValue::Number(-0.0)),
        ("u", HostValue::Undefined),
    ]);
    assert!(value.to_strict().is_err(), "undefined is not JSON");
    let strict = value.to_strict_omitting_undefined().unwrap();
    assert_eq!(strict["s"].as_string(), Some(&lone));
    assert!(
        strict
            .as_object()
            .unwrap()
            .get(&JsonString::from("u"))
            .is_none()
    );
    assert_eq!(
        pi_json::strict::stringify(&strict),
        r#"{"s":"a\ud800b","n":0}"#
    );
    let back = HostValue::from_strict(&strict);
    assert_eq!(back.get("s").and_then(HostValue::as_string), Some(&lone));
    assert!(HostValue::Number(f64::NAN).to_strict().is_err());
    let parsed = strict::parse(r#"["\udfff"]"#).unwrap();
    assert_eq!(HostValue::from_strict(&parsed).to_strict().unwrap(), parsed);
}

#[tokio::test]
async fn handles_keep_identity() {
    let (rt, _host) = runtime().await;
    let out = run(
        &rt,
        r#"
        import { call, callAsync, ref } from "pi-durable:bridge";
        class Thing { constructor() { this.x = 1; } }
        export async function run() {
            const fn = () => 1;
            const thing = new Thing();
            const plain = { a: 1 };
            const sym = Symbol("s");
            const map = new Map();
            const [f2, t2, p2, s2, m2, r2] = call("test.echo", fn, thing, plain, sym, map, ref(plain));
            const [af] = await callAsync("test.echoAsync", fn);
            const counter = call("test.counter", 41);
            const n = call("test.increment", counter);
            const n2 = call("test.increment", call("test.echo", counter)[0]);
            let wrongNative;
            try { call("test.increment", {}); } catch (e) { wrongNative = e instanceof TypeError; }
            return {
                fn: f2 === fn, thing: t2 === thing && t2 instanceof Thing, plainCopied: p2 !== plain && p2.a === 1,
                sym: s2 === sym, map: m2 === map, ref: r2 === plain, asyncFn: af === fn,
                counter: [n, n2, Object.getPrototypeOf(counter) === null], wrongNative,
            };
        }
        "#,
    )
    .await;
    assert_eq!(
        out,
        json!({
            "fn": true, "thing": true, "plainCopied": true, "sym": true, "map": true, "ref": true, "asyncFn": true,
            "counter": [42, 43, true], "wrongNative": true,
        })
    );
}

#[tokio::test]
async fn every_error_class_is_reconstructed() {
    let (rt, _host) = runtime().await;
    let names: Vec<&str> = ErrorClass::ALL.iter().map(|c| c.name()).collect();
    let out = run(
        &rt,
        r#"
        import * as bridge from "pi-durable:bridge";
        const { call, callAsync } = bridge;
        const cases = {
            Error: [{}, (e) => e.constructor === Error && e.message === "m"],
            TypeError: [{}, (e) => e instanceof TypeError && e.message === "m"],
            RangeError: [{}, (e) => e instanceof RangeError],
            AbortError: [{}, (e) => e instanceof DOMException && e.name === "AbortError" && e.message === "m"],
            ConversationBusy: [{ conversationId: 7 }, (e) => e instanceof bridge.ConversationBusy && e.conversationId === 7 && e.message === "Conversation 7 is busy"],
            ReadAfterWrite: [{ method: "get" }, (e) => e instanceof bridge.ReadAfterWrite && e.message === "Tx.get() cannot read tables after the first table write"],
            ReadOnlySession: [{ what: "commit" }, (e) => e instanceof bridge.ReadOnlySession && e.message === "Read-only Harness: commit refused"],
            SessionFailed: [{}, (e) => e instanceof bridge.SessionFailed && e.cause instanceof bridge.StorageRequestError && e.cause.message === "root cause"],
            StorageRequestError: [{}, (e) => e instanceof bridge.StorageRequestError && e.message === "m"],
            FileError: [{ code: "not_found", path: "/x" }, (e) => e instanceof bridge.FileError && e.code === "not_found" && e.path === "/x" && e.message === "m"],
            ExecutionError: [{ code: "timeout", spillPath: "/s" }, (e) => e instanceof bridge.ExecutionError && e.code === "timeout" && e.spillPath === "/s"],
            JsonlCorruptionError: [{}, (e) => e instanceof bridge.JsonlCorruptionError && e.message === "m"],
            JsonlStoragePoisonedError: [{}, (e) => e instanceof bridge.JsonlStoragePoisonedError && e.message === "JSONL storage is poisoned and must be reopened"],
            ChordError: [{ code: "invalid", reason: "args", details: { at: 1 } }, (e) => e instanceof bridge.ChordError && e.message === "invalid/args: m" && e.retryable === false && e.details.at === 1],
            PathError: [{ path: ["a", 1] }, (e) => e instanceof bridge.PathError && e.message === 'unresolvable path: ["a",1]'],
            UnsafePathError: [{ segment: "__proto__" }, (e) => e instanceof bridge.UnsafePathError && e.segment === "__proto__"],
            WatchError: [{}, (e) => e.name === "WatchError" && e.message === "m"],
        };
        export async function run() {
            const out = {};
            for (const [name, [props, check]] of Object.entries(cases)) {
                const cause = name === "SessionFailed";
                let sync, rejected;
                try { call("test.fail", name, "m", props, cause); } catch (e) { sync = check(e); }
                try { await callAsync("test.failAsync", name, "m", props, cause); } catch (e) { rejected = check(e); }
                out[name] = [sync, rejected];
            }
            let extra;
            try { call("test.fail", "FileError", "m", { code: "invalid", extra: 1 }); } catch (e) { extra = e.extra === 1 && Object.prototype.hasOwnProperty.call(e, "path"); }
            let lone;
            try { call("test.fail", "Error", "\ud800!", {}); } catch (e) { lone = e.message === "\ud800!"; }
            let unknown, asyncSync;
            try { call("no.such.method"); } catch (e) { unknown = e instanceof TypeError && /unknown pi-durable host method/.test(e.message); }
            try { call("test.echoAsync"); } catch (e) { asyncSync = e instanceof TypeError; }
            return { out, names: bridge.errorClassNames, extra, lone, unknown, asyncSync };
        }
        "#,
    )
    .await;
    assert_eq!(
        out["names"],
        json!(names),
        "bridge.js and ErrorClass::ALL must list the same classes"
    );
    for name in &names {
        assert_eq!(out["out"][name], json!([true, true]), "{name}");
    }
    assert_eq!(out["extra"], json!(true));
    assert_eq!(out["lone"], json!(true));
    assert_eq!(out["unknown"], json!(true));
    assert_eq!(out["asyncSync"], json!(true));
}

#[tokio::test]
async fn js_callbacks_reenter_and_keep_thrown_identity() {
    let (rt, _host) = runtime().await;
    let out = run(
        &rt,
        r#"
        import { call, callAsync, ref } from "pi-durable:bridge";
        export async function run() {
            const sync = call("test.callback", (x) => x.s + "\udc00", { s: "a" });
            const nested = call("test.callback", (x) => call("test.callback", (y) => y * 2, x + 1), 20);
            const viaPromise = call("test.callback", async (x) => x + 1, 1);
            const method = call("test.method", ref({ base: 10, add(n) { return this.base + n; } }), "add", 5);
            const failure = new Error("boom");
            let same;
            try { call("test.callback", () => { throw failure; }); } catch (e) { same = e === failure; }
            const asyncValue = await callAsync("test.callbackAsync", async (x) => { await new Promise((r) => setTimeout(r, 1)); return [x, undefined]; }, "\ud800");
            let asyncSame;
            try { await callAsync("test.callbackAsync", async () => { throw failure; }); } catch (e) { asyncSame = e === failure; }
            const blocking = await callAsync("test.callbackBlocking", async (x) => { await new Promise((r) => setTimeout(r, 1)); return x * 3; }, 7);
            let pending;
            try { call("test.callback", () => new Promise(() => {})); } catch (e) { pending = /pending/.test(e.message); }
            return { sync: sync === "a\udc00", nested, viaPromise, method, same, asyncValue: asyncValue[0] === "\ud800" && asyncValue.length === 2 && asyncValue[1] === undefined, asyncSame, blocking, pending };
        }
        "#,
    )
    .await;
    assert_eq!(
        out,
        json!({ "sync": true, "nested": 42, "viaPromise": 2, "method": 15, "same": true, "asyncValue": true, "asyncSame": true, "blocking": 21, "pending": true })
    );
}

#[tokio::test]
async fn module_table_maps_upstream_subpaths() {
    use pi_durable_jsrt::specifier_for;
    assert_eq!(
        specifier_for("index.js").as_deref(),
        Some("@earendil-works/pi-durable")
    );
    assert_eq!(
        specifier_for("env/index.js").as_deref(),
        Some("@earendil-works/pi-durable/env")
    );
    assert_eq!(
        specifier_for("storage/sqlite/node.js").as_deref(),
        Some("@earendil-works/pi-durable/storage/sqlite/node")
    );
    assert_eq!(
        specifier_for("package.json.js").as_deref(),
        Some("@earendil-works/pi-durable/package.json")
    );
    assert_eq!(
        specifier_for("chord/index.js").as_deref(),
        Some("@earendil-works/chord")
    );
    assert_eq!(
        specifier_for("chord/delta/index.js").as_deref(),
        Some("@earendil-works/chord/delta")
    );
    assert_eq!(
        specifier_for("chord/output.js").as_deref(),
        Some("@earendil-works/chord/output")
    );
    assert_eq!(
        specifier_for("internal/src/durable/session/session.js").as_deref(),
        Some("pi-durable:internal/src/durable/session/session")
    );
    assert_eq!(
        specifier_for("bridge.js").as_deref(),
        Some("pi-durable:bridge")
    );
    assert_eq!(specifier_for("vitest.js").as_deref(), Some("vitest"));
    let without: Vec<String> = pi_durable_jsrt::modules(false)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(without.contains(&"@earendil-works/chord/output".to_string()));
    assert!(!without.contains(&"vitest".to_string()));
}

#[tokio::test]
async fn vitest_shim_reports_each_outcome() {
    let (rt, _host) = runtime().await;
    rt.eval_module(
        r#"
        import { describe, it, test, expect, vi, beforeAll, afterAll, beforeEach, afterEach, onTestFinished } from "vitest";
        const log = [];
        globalThis.__log = log;
        beforeAll(() => { log.push("beforeAll"); return () => log.push("beforeAll-cleanup"); });
        afterAll(() => log.push("afterAll"));
        describe("suite", () => {
            beforeEach(() => log.push("beforeEach"));
            afterEach(() => log.push("afterEach-1"));
            afterEach(() => log.push("afterEach-2"));
            it("passes", () => {
                onTestFinished(() => log.push("finished"));
                expect({ a: [1, { b: undefined }], m: new Map([[1, { x: 2 }]]) }).toEqual({ a: [1, {}], m: new Map([[1, { x: 2 }]]) });
                expect({ a: 1, b: { c: 2, d: 3 } }).toMatchObject({ b: { c: 2 } });
                expect(-0).not.toEqual(0);
                expect([1, , 3]).not.toStrictEqual([1, undefined, 3]);
                expect(NaN).toEqual(NaN);
                expect({ when: expect.any(Number), name: expect.stringContaining("ell") }).toEqual({ when: 5, name: "hello" });
                expect(() => { throw new TypeError("bad thing"); }).toThrow(TypeError);
                expect(() => { throw new TypeError("bad thing"); }).toThrow(/bad/);
            });
            it("fails on mismatch", () => { expect(1).toBe(2); });
            it("rejects", async () => {
                await expect(Promise.reject(new RangeError("r"))).rejects.toThrow(RangeError);
                await expect(Promise.resolve(3)).resolves.toBe(3);
            });
            it("fails on unawaited rejection mismatch", async () => {
                await expect(Promise.resolve(1)).rejects.toThrow();
            });
            it.skip("skipped", () => { throw new Error("never"); });
            it.todo("todo");
            it.each([[1, 2, 3], [2, 3, 5]])("adds %i + %i", (a, b, c) => { expect(a + b).toBe(c); });
            it("times out", async () => { await new Promise(() => {}); }, 50);
            it.fails("expected failure", () => { expect(true).toBe(false); });
        });
        describe("fake timers", () => {
            afterEach(() => { vi.useRealTimers(); });
            it("advances", async () => {
                vi.useFakeTimers({ now: 1000 });
                const fired = [];
                setTimeout(() => fired.push(Date.now()), 100);
                setInterval(() => fired.push("i" + Date.now()), 40);
                expect(vi.getTimerCount()).toBe(2);
                vi.advanceTimersByTime(99);
                expect(fired).toEqual(["i1040", "i1080"]);
                await vi.advanceTimersByTimeAsync(1);
                expect(fired).toEqual(["i1040", "i1080", 1100]);
                vi.setSystemTime(5000);
                expect(Date.now()).toBe(5000);
                expect(new Date().getTime()).toBe(5000);
                expect(new Date(0).getTime()).toBe(0);
            });
            it("spies", async () => {
                const target = { greet(name) { return "hi " + name; } };
                const spy = vi.spyOn(target, "greet");
                expect(target.greet("a")).toBe("hi a");
                spy.mockReturnValueOnce("mocked");
                expect(target.greet("b")).toBe("mocked");
                expect(spy).toHaveBeenCalledTimes(2);
                expect(spy).toHaveBeenCalledWith("b");
                vi.restoreAllMocks();
                expect(vi.isMockFunction(target.greet)).toBe(false);
                const fn = vi.fn((x) => x * 2);
                expect(fn(2)).toBe(4);
                expect(fn.mock.calls).toEqual([[2]]);
                let ticks = 0;
                const timer = setInterval(() => ticks++, 5);
                await vi.waitFor(() => { if (ticks < 2) throw new Error("not yet"); }, { timeout: 2000, interval: 5 });
                clearInterval(timer);
            });
        });
        "#,
        "/vitest-selftest.test.mjs",
    )
    .await
    .unwrap();
    let vitest = rt.import("vitest").await.unwrap();
    let runner = vitest.get_handle("__runner").await.unwrap();
    let report = runner
        .call_method("run", vec![json!({ "file": "selftest" })])
        .await
        .unwrap();
    let statuses: Vec<(String, String)> = report["testResults"][0]["assertionResults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["fullName"].as_str().unwrap().to_string(),
                t["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let status = |name: &str| {
        statuses
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s.as_str())
            .unwrap_or("missing")
    };
    assert_eq!(status("suite passes"), "passed", "{report:#}");
    assert_eq!(status("suite fails on mismatch"), "failed");
    assert_eq!(status("suite rejects"), "passed", "{report:#}");
    assert_eq!(
        status("suite fails on unawaited rejection mismatch"),
        "failed"
    );
    assert_eq!(status("suite skipped"), "skipped");
    assert_eq!(status("suite todo"), "todo");
    assert_eq!(status("suite adds 1 + 2"), "passed");
    assert_eq!(status("suite adds 2 + 3"), "passed");
    assert_eq!(status("suite times out"), "failed");
    assert_eq!(status("suite expected failure"), "passed");
    assert_eq!(status("fake timers advances"), "passed", "{report:#}");
    assert_eq!(status("fake timers spies"), "passed", "{report:#}");
    assert_eq!(report["numFailedTests"], json!(3));
    assert_eq!(report["success"], json!(false));
    let log = rt.eval("globalThis.__log").await.unwrap();
    let log: Vec<String> = serde_json::from_value(log).unwrap();
    assert_eq!(
        &log[..5],
        [
            "beforeAll",
            "beforeEach",
            "afterEach-2",
            "afterEach-1",
            "finished"
        ]
    );
    assert_eq!(&log[log.len() - 2..], ["afterAll", "beforeAll-cleanup"]);
}

/// Import `path` with vitest's mock hoisting applied (as the runner does).
async fn import_hoisted(
    rt: &JsRuntime,
    path: &std::path::Path,
) -> pi_jsrt::Result<pi_jsrt::JsHandle> {
    let text = std::fs::read_to_string(path).unwrap();
    let hoisted = pi_durable_jsrt::hoist_vitest_mocks(&text, &path.display().to_string())
        .unwrap()
        .expect("file has vi.mock");
    rt.set_file_source(path.to_path_buf(), hoisted);
    rt.import(path.display().to_string()).await
}

#[tokio::test]
async fn vi_mock_is_hoisted_and_applied() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("helper.ts"),
        "import { readFileSync } from \"node:fs\";\nimport * as fsAll from \"fs\";\nexport const read = (p: string): string => readFileSync(p, \"utf8\");\nexport const viaNamespace = fsAll.readFileSync;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("local.ts"),
        "export const value = \"real\";\n",
    )
    .unwrap();
    let test = dir.path().join("mocked.test.ts");
    std::fs::write(
        &test,
        r#"import type * as Fs from "node:fs";
import { read, viaNamespace } from "./helper.ts";
import { value } from "./local.ts";
import { describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({ calls: 0, original: undefined as unknown }));

vi.mock("node:fs", async (importOriginal) => {
	const actual = await importOriginal<typeof Fs>();
	state.original = actual.readFileSync;
	return { ...actual, readFileSync: (...args: unknown[]) => { state.calls++; return "mocked"; } };
});
vi.mock("./local.ts", () => ({ value: "mock" }));

describe("mocks", () => {
	it("applies before imports load", async () => {
		expect(read("/nowhere")).toBe("mocked");
		expect(viaNamespace).not.toBe(state.original);
		expect(typeof state.original).toBe("function");
		expect(state.calls).toBe(1);
		expect(value).toBe("mock");
		const promises = await import("node:fs/promises");
		expect(typeof promises.readFile).toBe("function");
	});
});
"#,
    )
    .unwrap();
    let (rt, _host) = runtime().await;
    import_hoisted(&rt, &test).await.unwrap();
    let runner = rt
        .import("vitest")
        .await
        .unwrap()
        .get_handle("__runner")
        .await
        .unwrap();
    let report = runner.call_method("run", vec![json!({})]).await.unwrap();
    assert_eq!(report["numPassedTests"], json!(1), "{report:#}");
    assert_eq!(report["appliedMocks"], json!(["node:fs", "./local.ts"]));
    assert_eq!(report["unappliedMocks"], json!([]));
    // The file on disk is untouched.
    assert!(
        std::fs::read_to_string(&test)
            .unwrap()
            .starts_with("import type * as Fs")
    );
}

/// The pinned upstream file that uses `vi.mock`: its `node:fs` mock is built and installed before
/// its other imports load (those fail until chunk E provides the env modules).
#[tokio::test]
async fn upstream_env_node_spill_mock_is_applied() {
    let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../pi-mono/packages/durable/test/env-node-spill.test.ts");
    if !reference.exists() {
        eprintln!("skipped: {} not found", reference.display());
        return;
    }
    let (rt, _host) = runtime().await;
    let loaded = import_hoisted(&rt, &reference.canonicalize().unwrap()).await;
    let runner = rt
        .import("vitest")
        .await
        .unwrap()
        .get_handle("__runner")
        .await
        .unwrap();
    let mocks = runner.call_method("mocks", vec![]).await.unwrap();
    assert_eq!(
        mocks,
        json!({ "appliedMocks": ["node:fs"], "unappliedMocks": [] }),
        "load: {:?}",
        loaded.err()
    );
    let probe = rt
        .eval_module(
            r#"import * as fs from "node:fs";
            import { createWriteStream } from "fs";
            export const probe = () => {
                const mocked = globalThis[Symbol.for("pi-durable.vitest.mocks")].values().next().value;
                return [fs.createWriteStream === mocked.createWriteStream, createWriteStream === mocked.createWriteStream, typeof fs.readFileSync];
            };"#,
            "/probe.mjs",
        )
        .await
        .unwrap();
    assert_eq!(
        probe.call_method("probe", vec![]).await.unwrap(),
        json!([true, true, "function"])
    );
}
