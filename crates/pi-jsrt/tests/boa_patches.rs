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
    write(dir.path(), "b.mjs", "const b = 2\nexport { b as default, b, }");
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
    assert_eq!(ns.get("all").await.unwrap(), json!({ "a": 1, "b": 2, "c": 1 }));
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
    assert_eq!(ns.get("default").await.unwrap(), json!({ "called": [42, "none", 42], "arrows": [4, 7, 3, 3] }));
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
        json!([false, false, false, false, "function", "object", "function", "object", "function", "object"])
    );
}
