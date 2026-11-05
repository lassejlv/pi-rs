//! `node:vm`: contexts are QuickJS realms with V8-like contextified globals. The expected output is
//! Node 22's for the same script (run in Node mode by `pi-jsrt-node`), except that QuickJS
//! creates a script's `var` bindings before its function bindings (the order of `Object.keys` of
//! a contextified object after `var x; function f() {}`; V8 creates `f` first).

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

#[test]
fn vm_contexts_scripts_and_compile_function() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vm1.js");
    std::fs::write(&path, SCRIPT).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    assert_eq!(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), EXPECTED);
}

const SCRIPT: &str = r###"const vm = require("vm");
const sb = { a: 1, console };
vm.createContext(sb);
console.log(vm.runInContext("var x = 2; function f() { return a + x } y = 3; f()", sb), sb.x, sb.y, typeof sb.f);
console.log(vm.runInContext("this", sb) === sb, vm.runInContext("globalThis", sb) === sb, vm.isContext(sb), vm.isContext({}));
console.log(vm.runInContext("typeof toString, typeof Object, Object === this.Object, [1] instanceof Array", sb));
console.log(vm.runInNewContext("[1,2] instanceof Array"), vm.runInNewContext("[]") instanceof Array, Object.keys(sb));
console.log(vm.runInContext("let z = 5; z", sb), sb.z, vm.runInContext("z", sb));
const s = new vm.Script("a += 1; a", { filename: "s.js" }); console.log(s.runInContext(sb), s.runInContext(sb), s.runInNewContext({a: 10}), s.runInThisContext ? "has" : "");
try { vm.runInNewContext("throw new TypeError(\"x\")") } catch (e) { console.log(e instanceof TypeError, e.constructor.name, e.message) }
try { vm.runInNewContext("(") } catch (e) { console.log(e.name) }
const fn = vm.compileFunction("return a + b + c", ["a", "b"], { contextExtensions: [{ c: 100 }] }); console.log(fn(1, 2), JSON.stringify(fn.name), fn.length);
console.log(vm.runInThisContext("typeof require"), vm.runInNewContext("typeof process"));
try { vm.runInContext("1", {}) } catch (e) { console.log(e.name, e.code, e.message) }
console.log(vm.runInNewContext("this.constructor.name"));
const ctx2 = vm.createContext(); console.log(Object.keys(ctx2), vm.runInContext("var q = 1; this.q", ctx2), ctx2.q);
console.log(vm.runInNewContext("delete a; typeof a", { a: 1 }));
console.log(vm.runInNewContext("JSON.stringify({k: [1, new Date(0)]})"), vm.runInNewContext("Math.max(1, 5)"), vm.runInNewContext("x => x * 2")(21));
const later = vm.runInNewContext("(function () { return counter++; })", { counter: 0 });
console.log(later(), later());
const shared = { n: 0 }; vm.createContext(shared); vm.runInContext("function inc() { n++; }", shared); shared.inc(); shared.inc(); console.log(shared.n);
console.log(vm.runInNewContext("new Error('e').stack.split('\\n')[0]"), typeof vm.runInNewContext("Symbol.for('x')"));
console.log(vm.runInNewContext("var o = { p: 1 }; Object.getOwnPropertyNames(this).includes('o')"));
"###;

const EXPECTED: &str = r###"3 2 3 function
false false true false
true
true false [ 'a', 'console', 'x', 'f', 'y' ]
5 undefined 5
2 3 11 has
false TypeError x
SyntaxError
103 "" 2
undefined undefined
TypeError ERR_INVALID_ARG_TYPE The "contextifiedObject" argument must be an vm.Context. Received an instance of Object
Object
[] 1 1
undefined
{"k":[1,"1970-01-01T00:00:00.000Z"]} 5 42
0 1
2
Error: e symbol
true
"###;

/// `v8.setFlagsFromString("--expose-gc")` exposes `gc` in node:vm contexts created afterwards,
/// as V8 does (Chord watch.test.ts's `runInNewContext("gc")`).
#[tokio::test]
async fn expose_gc_flag_reaches_new_vm_contexts() {
    let rt = pi_jsrt::JsRuntime::new(pi_jsrt::RuntimeOptions {
        expose_gc: true,
        ..Default::default()
    })
    .unwrap();
    let value = rt
        .eval_module(
            r#"
        import { setFlagsFromString } from "node:v8";
        import { runInNewContext } from "node:vm";
        export const before = runInNewContext("typeof gc");
        setFlagsFromString("--expose-gc");
        export const after = runInNewContext("typeof gc");
        export const same = runInNewContext("gc") === globalThis.gc;
    "#,
            "/vm-gc.mjs",
        )
        .await
        .unwrap();
    assert_eq!(value.get("before").await.unwrap(), serde_json::json!("undefined"));
    assert_eq!(value.get("after").await.unwrap(), serde_json::json!("function"));
    assert_eq!(value.get("same").await.unwrap(), serde_json::json!(true));
}
