//! Regression tests for engine bugs fixed by the vendored Boa patches (`vendor/*/PATCH.md`).

use std::path::Path;

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

/// Zod memoizes shape getters by replacing them with data properties during the read.
#[tokio::test]
async fn self_replacing_getter_does_not_poison_property_cache() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const def = { shape: { a: 1 } };
        const original = def.shape;
        Object.defineProperty(def, 'shape', { get() {
            const value = { ...original };
            Object.defineProperty(def, 'shape', { value });
            return value;
        }});
        const read = () => def.shape;
        const results = [];
        for (let i = 0; i < 10; i++) results.push(read().a);
        return results;
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(value, json!([1, 1, 1, 1, 1, 1, 1, 1, 1, 1]));
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// boa_ast's scope analyzer skipped the computed key of class methods (including
/// accessors, static and async/generator methods), so functions created there could not
/// resolve module imports or top-level bindings (`ReferenceError`). `@anthropic-ai/sdk`
/// uses `[(_x = new WeakMap(), Symbol.asyncIterator)]()` style keys after down-leveling.
#[tokio::test]
async fn class_computed_method_key_closures_see_module_scope() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "t.mjs", "export const g = 7;\n");
    write(
        dir.path(),
        "main.mjs",
        r#"
import { g } from "./t.mjs";
const top = "top";
let f1, f2, f3, f4, f5, f6, f7, f8, f9;
class C {
  [(f1 = () => typeof g, "m")]() { return 1; }
  static [(f2 = () => g + 1, "s")]() { return 2; }
  get [(f3 = () => top, "acc")]() { return 3; }
  set [(f4 = () => g * 2, "acc")](v) {}
  async [(f5 = () => `${top}${g}`, "am")]() { return 5; }
  *[(f6 = () => [g, top], "gen")]() { yield 6; }
  [(f7 = () => typeof g, "fld")] = (f8 = () => g, 8);
  static { f9 = () => g - 1; }
  #p() { return g; }
  priv() { return this.#p(); }
}
const D = class Named {
  [(f10 = () => [typeof Named, g], "x")]() {}
};
var f10;
const o = { [(f11 = () => g, "k")]() {} };
var f11;
const c = new C();
export default {
  f1: f1(), f2: f2(), f3: f3(), f4: f4(), f5: f5(), f6: f6(), f7: f7(), f8: f8(), f9: f9(),
  f10: f10(), f11: f11(),
  m: c.m(), s: C.s(), acc: c.acc, am: await c.am(), gen: [...c.gen()], fld: c.fld, priv: c.priv(),
  d: typeof D.prototype.x,
};
"#,
    );
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let ns = rt
        .import(dir.path().join("main.mjs").to_str().unwrap())
        .await
        .unwrap_or_else(|e| panic!("{e}\n{:?}", e.as_js().and_then(|j| j.stack.clone())));
    let v = ns.get("default").await.unwrap();
    assert_eq!(
        v,
        json!({
            "f1": "number", "f2": 8, "f3": "top", "f4": 14, "f5": "top7", "f6": [7, "top"],
            "f7": "number", "f8": 7, "f9": 6, "f10": ["function", 7], "f11": 7,
            "m": 1, "s": 2, "acc": 3, "am": 5, "gen": [6], "fld": 8, "priv": 7, "d": "function"
        })
    );
}

/// The same pattern in a script (non-module) and inside a function scope.
#[tokio::test]
async fn class_computed_method_key_closures_in_function_scope() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let v = rt
        .eval(
            r#"(() => {
                const local = 41;
                let f;
                class K { [(f = () => local + 1, "m")]() {} }
                return f();
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(v, json!(42));
}

/// boa_parser required a token after `export { ... }` (to look for `from`), so a module ending in
/// a local export list without a semicolon failed with `SyntaxError: abrupt end`
/// (e.g. `fetch-blob/from.js`, used by node-fetch).
#[tokio::test]
async fn export_list_at_end_of_module_without_semicolon() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.mjs", "const a = 1\nexport { a }\n");
    write(
        dir.path(),
        "b.mjs",
        "const b = 2\nexport { b as default, b, }",
    );
    write(dir.path(), "c.mjs", "export { a } from './a.mjs'");
    write(
        dir.path(),
        "main.mjs",
        "import { a } from './a.mjs'\nimport b from './b.mjs'\nimport { a as c } from './c.mjs'\nconst all = { a, b, c }\nexport { all }",
    );
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let ns = rt
        .import(dir.path().join("main.mjs").to_str().unwrap())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        ns.get("all").await.unwrap(),
        json!({ "a": 1, "b": 2, "c": 1 })
    );
}

/// Boa's process-wide `Symbol.for` registry is not atomic; runtimes registering the same new keys in
/// parallel could get different symbols for one key (pi-jsrt serializes `Symbol.for`, see
/// `serialize_symbol_for`).
#[test]
fn symbol_for_is_consistent_across_parallel_runtimes() {
    const RUNTIMES: usize = 8;
    let prefix = format!("pi-jsrt-race-{}-", uuid::Uuid::new_v4());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(RUNTIMES));
    let threads: Vec<_> = (0..RUNTIMES)
        .map(|_| {
            let prefix = prefix.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                tokio.block_on(async move {
                    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
                    rt.register_function("host.barrier", move |_, _| {
                        barrier.wait();
                        Ok(serde_json::Value::Null)
                    })
                    .await
                    .unwrap();
                    rt.eval(format!(
                        r#"(() => {{
                            const keys = Array.from({{ length: 20000 }}, (_, i) => "{prefix}" + i);
                            host.barrier();
                            const first = keys.map((key) => Symbol.for(key));
                            let mismatches = 0;
                            keys.forEach((key, i) => {{ if (Symbol.for(key) !== first[i] || Symbol.keyFor(first[i]) !== key) mismatches++; }});
                            return mismatches;
                        }})()"#
                    ))
                    .await
                    .unwrap()
                })
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), json!(0));
    }
}

/// vendor/boa_parser: `async(x)` not followed by `=>` is a call of a function named `async`
/// (`asynckit`, used by form-data and axios), not an async arrow head.
#[tokio::test]
async fn call_of_a_function_named_async() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let ns = rt
        .eval_module(
            r#"
            function async(cb) { return typeof cb === "function" ? cb() + 1 : "none"; }
            const iterator = (item, f) => f;
            const cb = () => 41;
            const called = [async(cb), async(), iterator(1, async(cb))];
            const arrows = [await (async (x) => x * 2)(2), await (async () => 7)(), await (async x => x)(3), await (async (a, b) => a + b)(1, 2)];
            export default { called, arrows };
            "#,
            "async-call.mjs",
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        ns.get("default").await.unwrap(),
        json!({ "called": [42, "none", 42], "arrows": [4, 7, 3, 3] })
    );
}

/// Boa's global-name inline cache records the slot found by a lookup with the shape the global
/// object has *after* the lookup. Lazily loaded globals used to replace their accessor with a data
/// property inside the getter, so the second `x instanceof ReadableStream` (same code site) "called"
/// the class as the cached getter. Lazy globals now stay accessors (see prelude.js).
#[tokio::test]
async fn lazy_globals_survive_repeated_access_from_one_site() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let ns = rt
        .eval_module(
            r#"
            const out = [];
            for (const v of ["x", 1, undefined, {}]) out.push(v instanceof ReadableStream);
            for (let i = 0; i < 3; i++) out.push(typeof Headers, typeof WebAssembly);
            export default out;
            "#,
            "lazy-globals.mjs",
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        ns.get("default").await.unwrap(),
        json!([
            false, false, false, false, "function", "object", "function", "object", "function",
            "object"
        ])
    );
}

/// `ArraySetLength` truncates when `splice` shrinks an array a coercion grew (Chord tracker test).
#[tokio::test]
async fn splice_truncates_elements_added_by_a_coercion() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const values = [1, 2, 3];
        const removed = values.splice({ valueOf() { values.push(4); return 1; } }, 1, 9);
        return { values, keys: Object.keys(values), length: values.length, removed };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({ "values": [1, 9, 3], "keys": ["0", "1", "2"], "length": 3, "removed": [2] })
    );
}

/// A dead persistent register from `for (let …)` kept a temporary alive across `await`
/// (Chord retention.worker.ts `obsolete-revisions`).
#[tokio::test]
async fn for_let_loop_does_not_keep_a_dead_temporary_alive_across_await() {
    let rt = JsRuntime::new(RuntimeOptions {
        expose_gc: true,
        ..Default::default()
    })
    .unwrap();
    let value = rt
        .eval(
            r#"(async () => {
        async function collect(ref) {
            for (let attempt = 0; attempt < 10; attempt++) {
                await null;
                gc();
                if (ref.deref() === undefined) return "freed";
            }
            return "retained";
        }
        async function run() {
            const t = { value: { rows: [1, 2, 3] } };
            const ref = new WeakRef(t.value.rows);
            for (let i = 0; i < 3; i++) {}
            t.value = null;
            return await collect(ref);
        }
        return await run();
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(value, json!("freed"));
}

/// `JsStr::index_of` walked every window before `fromIndex`, so `split`, `replaceAll` and
/// `indexOf`/`includes` with a position were quadratic: splitting 200,000 lines took seconds in
/// release builds (durable `codemode.test.ts`, "keeps a result inside the tool's output limits").
#[tokio::test]
async fn string_search_from_a_position_is_linear() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let started = std::time::Instant::now();
    let value = rt
        .eval(
            r#"(() => {
        const text = Array.from({ length: 200000 }, () => "a").join("\n");
        const s = "abcabcéabc";
        return [
            text.split("\n").length,
            text.replaceAll("\n", "").length,
            s.indexOf("abc", 1), s.indexOf("abc", 7), s.indexOf("abc", 11), s.indexOf("", 10), s.indexOf("", 11),
            s.includes("abc", 8), s.includes("c", 10),
            "a,b,,c".split(","), "ab".split("", 1), "x".split("x"),
        ];
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!([
            200000,
            200000,
            3,
            7,
            -1,
            10,
            10,
            false,
            false,
            ["a", "b", "", "c"],
            ["a"],
            ["", ""]
        ])
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

/// A site that resets a shared array (`beforeEach(() => { log.length = 0; })`) runs again with the
/// `length` slot cached; it must still delete the elements.
#[tokio::test]
async fn cached_array_length_set_still_truncates() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const log = [];
        const rounds = [];
        for (let round = 0; round < 3; round++) {
            for (let i = 0; i < 4; i++) log.push(i);
            log.length = 0;
            log.push("a");
            rounds.push([Reflect.ownKeys(log).length, Object.keys(log).length, log.length, 2 in log]);
        }
        return rounds;
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!([[2, 1, 1, false], [2, 1, 1, false], [2, 1, 1, false]])
    );
}

/// `%TypedArray%.prototype.set` from a dense array of numbers takes a fast path that converts and
/// stores the elements directly; results must match the generic per-element path, which every
/// other source (holes, getters, non-numbers, BigInt arrays, proxies) still takes.
#[tokio::test]
async fn typed_array_set_from_dense_numbers_matches_the_generic_path() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const out = {};
        const u8 = new Uint8Array(6);
        u8.set([256, -1, 1.7, 255], 1);
        out.u8 = [...u8];
        const clamped = new Uint8ClampedArray(4);
        clamped.set([300, -5, 1.5, 2.5]);
        out.clamped = [...clamped];
        const f32 = new Float32Array(3);
        f32.set([0.1, NaN, -0]);
        out.f32 = [f32[0] === Math.fround(0.1), Number.isNaN(f32[1]), Object.is(f32[2], -0)];
        const i16 = new Int16Array(2);
        i16.set([40000, -40000]);
        out.i16 = [...i16];
        const holes = new Uint8Array(3).fill(9);
        holes.set([1, , 3]);
        out.holes = [...holes];
        const mixed = new Uint8Array(2);
        mixed.set(["7", { valueOf: () => 8 }]);
        out.mixed = [...mixed];
        const getter = [1, 2];
        Object.defineProperty(getter, 1, { get: () => 42 });
        const fromGetter = new Uint8Array(2);
        fromGetter.set(getter);
        out.getter = [...fromGetter];
        const big = new BigInt64Array(2);
        big.set([1n, -2n]);
        out.big = [...big].map(String);
        try { new Uint8Array(2).set([1, 2], 1); out.range = "none"; } catch (e) { out.range = e instanceof RangeError; }
        const proxy = new Uint8Array(2);
        proxy.set(new Proxy([5, 6], {}));
        out.proxy = [...proxy];
        return out;
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({
            "u8": [0, 0, 255, 1, 255, 0], "clamped": [255, 0, 2, 2], "f32": [true, true, true],
            "i16": [-25536, 25536], "holes": [1, 0, 3], "mixed": [7, 8], "getter": [1, 42],
            "big": ["1", "-2"], "range": true, "proxy": [5, 6],
        })
    );
}

/// Every way of shrinking an array deletes the indices at or above the new length, in every
/// storage mode (dense, sparse, with holes, after a transition) and through every set path
/// (cached named set, computed key, `Reflect.set`, `defineProperty`, subclass).
#[tokio::test]
async fn array_length_truncation_in_every_storage_mode() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const keys = (a) => [a.length, Reflect.ownKeys(a).filter((k) => k !== "length")];
        const out = {};
        const log = [];
        for (let round = 0; round < 3; round++) { for (let i = 0; i < 9; i++) log.push(i); log.length = 0; log.push("a", "b"); }
        out.cachedLoop = keys(log);
        const sparse = []; sparse[0] = 1; sparse[100] = 2; sparse[1000] = 3; sparse.length = 50;
        out.sparse = keys(sparse);
        const holes = [1, 2, 3, 4, 5, 6]; delete holes[1]; holes.length = 3;
        out.holes = keys(holes);
        const viaReflect = [1, 2, 3, 4]; Reflect.set(viaReflect, "length", 1);
        out.reflect = keys(viaReflect);
        const viaDefine = [1, 2, 3, 4]; Object.defineProperty(viaDefine, "length", { value: 2 });
        out.define = keys(viaDefine);
        const setComputed = (a, n) => { a["length"] = n; };
        let computed; for (let k = 0; k < 4; k++) { computed = [1, 2, 3, 4, 5]; setComputed(computed, 2); }
        out.computed = keys(computed);
        const reset = (a) => { a.length = 0; };
        let reused; for (let k = 0; k < 4; k++) { reused = [1, 2, 3]; reset(reused); reused.push(9); }
        out.functionCache = keys(reused);
        class Sub extends Array {}
        const sub = Sub.from([1, 2, 3, 4]); for (let k = 0; k < 3; k++) sub.length = 1;
        out.subclass = keys(sub);
        return out;
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({
            "cachedLoop": [2, ["0", "1"]], "sparse": [50, ["0"]], "holes": [3, ["0", "2"]],
            "reflect": [1, ["0"]], "define": [2, ["0", "1"]], "computed": [2, ["0", "1"]],
            "functionCache": [1, ["0"]], "subclass": [1, ["0"]],
        })
    );
}

/// Typed array property names that cannot be canonical numeric strings skip the numeric check;
/// the ones that can (`-0`, `1.5`, `Infinity`, `NaN`, `-1`) still never become own properties.
#[tokio::test]
async fn typed_array_canonical_numeric_keys_are_still_integer_indexed() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const a = new Uint8Array(2);
        for (const key of ["-0", "1.5", "Infinity", "NaN", "-1", "-Infinity"]) a[key] = 7;
        a.name = "kept"; a["1e3"] = 5; a["01"] = 6;
        return { keys: Reflect.ownKeys(a), name: a.name, has: "-0" in a, length: a.length, set: typeof a.set };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({ "keys": ["0", "1", "name", "1e3", "01"], "name": "kept", "has": false, "length": 2, "set": "function" })
    );
}

/// `JSON.parse` without a reviver builds the value directly; it must match the evaluated
/// path exactly: lone surrogates, `__proto__` as an own key, duplicate keys (first position,
/// last value), `-0`, integer-like key order, overflow, errors, and the reviver path.
#[tokio::test]
async fn json_parse_direct_builder_matches_spec_semantics() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let value = rt
        .eval(
            r#"(() => {
        const a = JSON.parse('{"a":1,"d":"x\\u00e9\\ud800z","g":-0,"h":1e400,"i":-1.5e-7,"n":null,"t":[true,false,[]]}');
        const p = JSON.parse('{"__proto__":{"x":1},"k":1,"k":2,"b":3}');
        const o = JSON.parse('{"1":"one","0":"zero","b":"b","a":"a"}');
        const errors = ['{', '[1,]', '{"a" 1}', 'tru', '"\\x"', '01'].map((t) => { try { JSON.parse(t); return "accepted"; } catch (e) { return e.name; } });
        return {
            units: [...a.d].map((c) => c.charCodeAt(0)),
            negativeZero: Object.is(a.g, -0), overflow: a.h === Infinity, small: a.i, nested: a.t, nul: a.n,
            ownProto: Object.hasOwn(p, "__proto__") && Object.getPrototypeOf(p) === Object.prototype && p.__proto__.x === 1,
            duplicate: [Object.keys(p), p.k],
            order: Object.keys(o),
            errors,
            reviver: JSON.parse('{"a":[1,2]}', (k, v) => (typeof v === "number" ? v * 10 : v)),
        };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({
            "units": [120, 233, 55296, 122],
            "negativeZero": true, "overflow": true, "small": -1.5e-7, "nested": [true, false, []], "nul": null,
            "ownProto": true,
            "duplicate": [["__proto__", "k", "b"], 2],
            "order": ["0", "1", "b", "a"],
            "errors": ["SyntaxError", "SyntaxError", "SyntaxError", "SyntaxError", "SyntaxError", "SyntaxError"],
            "reviver": {"a": [10, 20]},
        })
    );
}

/// An arrow created in a constructor (or any function with a function scope) must not keep the
/// receiver alive when nothing reads `this` through the environment (embedded-c's Session
/// retention reader); `this`, `super` and `new.target` in arrows still work.
#[tokio::test]
async fn arrows_that_do_not_use_this_do_not_retain_the_receiver() {
    let rt = JsRuntime::new(RuntimeOptions {
        expose_gc: true,
        ..Default::default()
    })
    .unwrap();
    let value = rt
        .eval(
            r#"(async () => {
        const held = [];
        class Captures { constructor(f) { held.push(() => f()); } }
        class Nothing { constructor() { held.push(() => 1); } }
        function Plain(f) { held.push(() => f()); }
        class UsesThis { constructor() { held.push(() => this); } }
        const make = (K) => new WeakRef(new K(() => 1));
        const refs = { Captures: make(Captures), Nothing: make(Nothing), Plain: make(Plain), UsesThis: make(UsesThis) };
        for (let i = 0; i < 5; i++) { await null; gc(); }
        const status = Object.fromEntries(Object.entries(refs).map(([k, r]) => [k, r.deref() === undefined ? "collected" : "retained"]));
        class Base { hi() { return "base"; } }
        class Derived extends Base { constructor() { super(); this.h = () => super.hi() + (this instanceof Derived); } }
        function Target() { this.t = () => new.target === Target; }
        const nested = { m() { return (() => () => (async () => () => this)())(); } };
        class Nests { constructor() { this.n = 7; } read() { return ((e) => [1].map(() => this.n + e))(1)[0]; } }
        const semantics = [new Derived().h(), new Target().t(), (function () { return (() => this)(); }).call(5) == 5,
          (await nested.m()())() === nested, new Nests().read()];
        return { status, semantics };
    })()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value,
        json!({
            "status": { "Captures": "collected", "Nothing": "collected", "Plain": "collected", "UsesThis": "retained" },
            "semantics": ["basetrue", true, true, true, 8],
        })
    );
}

/// Boa ended deep recursion with an uncatchable engine error, and a handler that caught an error
/// thrown deep in the stack kept the unwound frames' stack space, so the next calls overflowed.
/// V8 and QuickJS throw a `RangeError` the script can catch and then carry on.
#[tokio::test]
async fn deep_recursion_throws_a_catchable_range_error_and_unwinds() {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    for wrapper in ["(() => {BODY})()", "(async () => {BODY})()"] {
        let body = r#"
            let depth = 0;
            function dive() { depth++; dive(); }
            function climb(n) { return n === 0 ? 0 : 1 + climb(n - 1); }
            const caught = [];
            for (let i = 0; i < 3; i++) {
                depth = 0;
                try { dive(); } catch (error) { caught.push([error instanceof RangeError, error.message, depth > 1000]); }
            }
            function thrower(n) { if (n === 0) throw new Error("deep"); thrower(n - 1); }
            for (let i = 0; i < 50; i++) { try { thrower(2000); } catch {} }
            return [caught, climb(2000)];
        "#;
        let value = rt.eval(wrapper.replace("BODY", body)).await.unwrap();
        let entry = json!([true, "Maximum call stack size exceeded", true]);
        assert_eq!(value, json!([[entry.clone(), entry.clone(), entry], 2000]), "{wrapper}");
    }
}
