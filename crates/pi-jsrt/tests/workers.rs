//! `worker_threads` (each Worker another runtime on its own thread), `MessageChannel`/
//! `MessagePort` across threads with transfers, `BroadcastChannel`, `receiveMessageOnPort`.
//! Scripts run in Node mode (`pi-jsrt-node`); the expected output is Node 22's.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

fn run(name: &str, script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, script).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).current_dir(dir.path()).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    text.replace(&path.display().to_string(), "SCRIPT")
}

#[test]
fn workers_messages_errors_and_limits() {
    let script = r###"const { Worker, isMainThread, parentPort, workerData, threadId, resourceLimits } = require("worker_threads");
if (isMainThread) {
  const w = new Worker(__filename, { workerData: { n: 1, buf: new Uint8Array([1]) }, resourceLimits: { maxOldGenerationSizeMb: 64 } });
  console.log("main", threadId, typeof w.threadId, w.resourceLimits);
  w.on("online", () => console.log("online"));
  w.on("message", (m) => { console.log("msg", m); if (m === "ready") w.postMessage({ ping: 1 }); });
  w.on("error", (e) => console.log("error", e.constructor.name, e.message, e.code));
  w.on("exit", (c) => console.log("exit", c, w.resourceLimits));
} else {
  console.log("worker", threadId, workerData, resourceLimits, process.argv.length, typeof process.send);
  parentPort.postMessage("ready");
  parentPort.once("message", (m) => { parentPort.postMessage({ got: m }); setTimeout(() => { throw new RangeError("worker boom"); }, 10); });
}
"###;
    let expected = r###"main 0 number {
  maxYoungGenerationSizeMb: -1,
  maxOldGenerationSizeMb: 64,
  codeRangeSizeMb: -1,
  stackSizeMb: 4
}
online
worker 1 { n: 1, buf: Uint8Array(1) [ 1 ] } {
  maxYoungGenerationSizeMb: 48,
  maxOldGenerationSizeMb: 64,
  codeRangeSizeMb: 0,
  stackSizeMb: 4
} 2 undefined
msg ready
msg { got: { ping: 1 } }
error RangeError worker boom undefined
exit 1 {}
"###;
    assert_eq!(run("w.js", script), expected);
}

#[test]
fn worker_validation_terminate_exit_codes() {
    let script = r###"const { Worker } = require("worker_threads");
for (const f of [() => new Worker("x.js"), () => new Worker(5), () => new Worker("console.log(1)", {eval: true, workerData: () => 1})]) { try { f() } catch (e) { console.log(e.name, e.code, e.message) } }
const w = new Worker("setInterval(()=>{}, 1000); require(\"worker_threads\").parentPort.postMessage(process.env.X)", {eval: true, env: {X: "y"}});
w.on("message", async (m) => { console.log("m", m); console.log("term", await w.terminate()); });
w.on("exit", (c) => console.log("exit", c));
const w2 = new Worker("process.exit(4)", {eval: true}); w2.on("exit", (c) => console.log("exit2", c));
const w3 = new Worker("import(\"node:fs\").then(fs => console.log(typeof fs.readFileSync)); Promise.reject(new Error(\"rej\"))", {eval: true}); w3.on("error", (e) => console.log("err3", e.message)); w3.on("exit", (c) => console.log("exit3", c));
"###;
    let expected = r###"TypeError ERR_WORKER_PATH The worker script or module filename must be an absolute path or a relative path starting with './' or '../'. Received "x.js"
TypeError ERR_INVALID_ARG_TYPE The "filename" argument must be of type string or an instance of URL. Received type number (5)
DataCloneError 25 () => 1 could not be cloned.
exit2 4
m y
exit 1
term 1
function
err3 rej
exit3 1
"###;
    // Three workers run concurrently: their lines interleave differently between runs.
    let sorted = |s: &str| {
        let mut v: Vec<String> = s.lines().map(String::from).collect();
        v.sort();
        v
    };
    assert_eq!(sorted(&run("w2.js", script)), sorted(expected));
}

#[test]
fn message_ports_broadcast_channels_and_transfers() {
    let script = r###"const { port1, port2 } = new MessageChannel();
for (const f of [() => port1.postMessage(1, [port1]), () => port1.postMessage({p: new MessageChannel().port1}), () => { const c = new MessageChannel(); port1.postMessage(c.port1, [c.port1]); port1.postMessage(c.port1, [c.port1]); }, () => { const ab = new ArrayBuffer(4); port1.postMessage(ab, [ab, ab]) }, () => port1.postMessage(() => 1)]) { try { f(); console.log("ok") } catch (e) { console.log(e.constructor.name, e.name, e.message) } }
const ab = new ArrayBuffer(8); port1.postMessage({ab}, [ab]); console.log("detached", ab.byteLength);
port2.on("message", (m) => { console.log("got", m instanceof MessagePort ? "port" : m.ab ? ["ab", m.ab.byteLength] : m); });
port2.on("close", () => console.log("close2")); port1.on("close", () => console.log("close1"));
const { receiveMessageOnPort } = require("worker_threads"); const c = new MessageChannel(); c.port1.postMessage({x: 1}); console.log(receiveMessageOnPort(c.port2), receiveMessageOnPort(c.port2)); c.port1.close();
const bc1 = new BroadcastChannel("b"), bc2 = new BroadcastChannel("b"); bc2.onmessage = (e) => { console.log("bc", e.data, e.constructor.name); bc1.close(); bc2.close(); }; bc1.postMessage({hi: 1});
// port passed to a worker
const { Worker } = require("worker_threads");
const ch = new MessageChannel();
const w = new Worker(`const { workerData } = require("worker_threads"); workerData.port.postMessage("via transferred port " + workerData.n); workerData.port.close();`, { eval: true, workerData: { port: ch.port2, n: 7 }, transferList: [ch.port2] });
ch.port1.on("message", (m) => console.log(m));
ch.port1.on("close", () => console.log("transferred port closed"));
w.on("exit", (c) => { console.log("worker exit", c); port1.close(); });
"###;
    let expected = r###"DOMException DataCloneError Transfer list contains source port
DOMException DataCloneError Object that needs transfer was found in message but not listed in transferList
DOMException DataCloneError MessagePort in transfer list is already detached
DOMException DataCloneError Transfer list contains duplicate ArrayBuffer
DOMException DataCloneError () => 1 could not be cloned.
detached 0
{ message: { x: 1 } } undefined
got port
got [ 'ab', 8 ]
bc { hi: 1 } MessageEvent
via transferred port 7
transferred port closed
worker exit 0
close1
close2
"###;
    assert_eq!(run("mp.js", script), expected);
}

#[test]
fn esm_worker_with_transferred_data_and_stdout() {
    let script = r###"// ESM worker with a module file, workerData with transferables and shared env data
import { Worker, setEnvironmentData, isMainThread, parentPort, workerData, getEnvironmentData, threadId } from "node:worker_threads";
import { fileURLToPath } from "node:url";
if (isMainThread) {
  setEnvironmentData("k", { v: 1 });
  const ab = new Uint8Array([1, 2, 3]).buffer;
  const w = new Worker(new URL(import.meta.url), { workerData: { ab, map: new Map([[1, new Set(["x"])]]), date: new Date(5), err: new TypeError("t") }, transferList: [ab], stdout: true });
  console.log("transferred", ab.byteLength);
  let out = "";
  w.stdout.on("data", (d) => (out += d));
  w.on("message", (m) => console.log("message", JSON.stringify(m)));
  w.on("exit", (c) => console.log("exit", c, JSON.stringify(out)));
} else {
  const d = workerData;
  console.log("from worker", threadId);
  parentPort.postMessage({ len: d.ab.byteLength, bytes: [...new Uint8Array(d.ab)], map: [...d.map.get(1)], date: d.date.getTime(), err: [d.err.name, d.err.message, d.err instanceof TypeError], env: getEnvironmentData("k") });
}
"###;
    let expected = r###"transferred 0
message {"len":3,"bytes":[1,2,3],"map":["x"],"date":5,"err":["TypeError","t",true],"env":{"v":1}}
exit 0 "from worker 1\n"
"###;
    assert_eq!(run("w3.mjs", script), expected);
}

#[test]
fn v8_serialize_is_byte_compatible() {
    let script = r###"const v8 = require("v8");
const cases = {
  num: 1, neg: -5, dbl: 1.5, big: 2**40, negz: -0, nan: NaN, str: "abc", latin: "é", two: "€uro", empty: "",
  t: true, f: false, n: null, u: undefined, bi: 123n, nbi: -(2n**70n), obj: {a: 1, 2: "x", b: [1,2]}, arr: [1, "a", null], holey: [1,,3], sparse: (()=>{const a=[]; a[5]=1; a.x=2; return a})(),
  date: new Date(0), re: /ab+c/gi, map: new Map([[1, "a"], ["k", {}]]), set: new Set([1, "x"]),
  buf: Buffer.from([1,2,3]), u8: new Uint8Array([4,5]), f64: new Float64Array([1.5]), dv: new DataView(new ArrayBuffer(2)), ab: new ArrayBuffer(3), i16: new Int16Array([1]), b64: new BigInt64Array([1n]), bu64: new BigUint64Array([2n]),
  err: Object.assign(new RangeError("bad"), {stack: "RangeError: bad\n    at x"}), errc: Object.assign(new Error("m", {cause: 5}), {stack: "s"}),
  wrap: [new Number(3), new String("s"), new Boolean(true), new Boolean(false), Object(5n)],
  cyc: (()=>{const o={}; o.self=o; o.list=[o]; return o})(),
  long: "x".repeat(200), twolong: "€".repeat(100), nested: { a: { b: { c: [new Map([[{}, new Set([[]])]])] } } },
};
const out = {};
for (const [k, v] of Object.entries(cases)) { try { out[k] = v8.serialize(v).toString("hex"); } catch (e) { out[k] = ["ERR", e.name, e.message]; } }
const round = {};
for (const [k, hex] of Object.entries(out)) { if (Array.isArray(hex)) continue; try { const v = v8.deserialize(Buffer.from(hex, "hex")); round[k] = v8.serialize(v).toString("hex") === hex; } catch (e) { round[k] = ["ERR", e.message]; } }
const errs = [];
for (const v of [() => 1, function foo() {}, Symbol("x"), Promise.resolve(), new WeakMap()]) { try { v8.serialize(v) } catch (e) { errs.push([e.constructor.name, e.message]) } try { structuredClone(v) } catch (e) { errs.push([e.constructor.name, e.name, e.message]) } }
const s = new v8.Serializer(); s.writeHeader(); s.writeUint32(300); s.writeUint64(1, 2); s.writeDouble(0.5); s.writeRawBytes(Buffer.from("hi")); s.writeValue({x: 1});
const sb = s.releaseBuffer();
const d = new v8.Deserializer(sb); d.readHeader();
const custom = [sb.toString("hex"), d.getWireFormatVersion(), d.readUint32(), d.readUint64(), d.readDouble(), d.readRawBytes(2).toString(), d.readValue()];
let bad; try { v8.deserialize(Buffer.from("ff0f6f", "hex")); } catch (e) { bad = e.message; }
console.log(JSON.stringify({ out, round, errs, custom, bad }));
"###;
    let expected = r###"{"out":{"num":"ff0f4902","neg":"ff0f4909","dbl":"ff0f4e000000000000f83f","big":"ff0f4e0000000000007042","negz":"ff0f4e0000000000000080","nan":"ff0f4e000000000000f87f","str":"ff0f2203616263","latin":"ff0f2201e9","two":"ff0f6308ac20750072006f00","empty":"ff0f2200","t":"ff0f54","f":"ff0f46","n":"ff0f30","u":"ff0f5f","bi":"ff0f5a107b00000000000000","nbi":"ff0f5a2100000000000000004000000000000000","obj":"ff0f6f490422017822016149022201624102490249042400027b03","arr":"ff0f4103490222016130240003","holey":"ff0f61034900490249044906400203","sparse":"ff0f6106490a49022201784904400206","date":"ff0f440000000000000000","re":"ff0f52220461622b6303","map":"ff0f3b490222016122016b6f7b003a04","set":"ff0f2749022201782c02","buf":"ff0f5c0a03010203","u8":"ff0f5c01020405","f64":"ff0f5c0808000000000000f83f","dv":"ff0f5c09020000","ab":"ff0f4203000000","i16":"ff0f5c03020100","b64":"ff0f5c0b080100000000000000","bu64":"ff0f5c0c080200000000000000","err":"ff0f72526d220362616473221852616e67654572726f723a206261640a20202020617420782e","errc":"ff0f726d22016d7322017363490a2e","wrap":"ff0f41056e00000000000008407322017379787a100500000000000000240005","cyc":"ff0f6f220473656c665e0022046c69737441015e002400017b02","long":"ff0f22c8017878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878","twolong":"ff0f0063c801ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20ac20","nested":"ff0f6f2201616f2201626f22016341013b6f7b002741002400002c013a022400017b017b017b01"},"round":{"num":true,"neg":true,"dbl":true,"big":true,"negz":true,"nan":true,"str":true,"latin":true,"two":true,"empty":true,"t":true,"f":true,"n":true,"u":true,"bi":true,"nbi":true,"obj":false,"arr":false,"holey":true,"sparse":true,"date":true,"re":true,"map":true,"set":true,"buf":true,"u8":true,"f64":true,"dv":true,"ab":true,"i16":true,"b64":true,"bu64":true,"err":true,"errc":true,"wrap":false,"cyc":false,"long":true,"twolong":true,"nested":false},"errs":[["Error","() => 1 could not be cloned."],["DOMException","DataCloneError","() => 1 could not be cloned."],["Error","function foo() {} could not be cloned."],["DOMException","DataCloneError","function foo() {} could not be cloned."],["Error","Symbol(x) could not be cloned."],["DOMException","DataCloneError","Symbol(x) could not be cloned."],["Error","#<Promise> could not be cloned."],["DOMException","DataCloneError","#<Promise> could not be cloned."],["Error","#<WeakMap> could not be cloned."],["DOMException","DataCloneError","#<WeakMap> could not be cloned."]],"custom":["ff0fac028280808010000000000000e03f68696f22017849027b01",15,300,[1,2],0.5,"hi",{"x":1}],"bad":"Unable to deserialize cloned data."}
"###;
    assert_eq!(run("ser2.js", script), expected);
}

#[test]
fn fork_with_advanced_serialization() {
    let script = r###"const { fork } = require("child_process");
if (process.argv[2] === "child") {
  process.on("message", (m) => { process.send({ back: m, types: [m.map instanceof Map, m.d instanceof Date, Buffer.isBuffer(m.b), typeof m.big] }); process.disconnect(); });
} else {
  const opts = process.env.CHILD_EXEC ? { execPath: process.env.CHILD_EXEC } : {};
  const c = fork(__filename, ["child"], { ...opts, serialization: "advanced" });
  c.send({ map: new Map([[1, 2]]), d: new Date(7), b: Buffer.from("hi"), big: 10n, nested: [new Set([1])] });
  c.on("message", (m) => console.log(require("util").inspect(m, { depth: 4, breakLength: Infinity })));
}
"###;
    let expected = r###"{ back: { map: Map(1) { 1 => 2 }, d: 1970-01-01T00:00:00.007Z, b: <Buffer 68 69>, big: 10n, nested: [ Set(1) { 1 } ] }, types: [ true, true, true, 'bigint' ] }
"###;
    assert_eq!(run("adv.js", script), expected);
}
