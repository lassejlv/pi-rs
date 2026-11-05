//! `node:perf_hooks` and the `performance` global: marks/measures and their errors,
//! PerformanceObserver (async delivery sorted by start time), timerify, histograms,
//! monitorEventLoopDelay, eventLoopUtilization, nodeTiming. Expected output is Node 22's.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

#[test]
fn perf_hooks_match_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("perf.js");
    std::fs::write(&path, SCRIPT).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    assert_eq!(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), EXPECTED);
}

const SCRIPT: &str = r###"const { performance, PerformanceObserver, monitorEventLoopDelay, createHistogram } = require("perf_hooks");
const log = [];
log.push(["same", performance === globalThis.performance, typeof PerformanceObserver, PerformanceObserver.supportedEntryTypes]);
const m = performance.mark("a", { detail: { x: 1 } });
log.push(["mark", m.constructor.name, m.name, m.entryType, m.duration, m.detail, typeof m.startTime, Object.keys(m.toJSON())]);
const obs = new PerformanceObserver((list, observer) => {
  log.push(["observed", list.getEntries().map((e) => [e.entryType, e.name]), list.getEntriesByType("measure").length, observer === obs]);
});
obs.observe({ entryTypes: ["mark", "measure"] });
performance.mark("b");
const meas = performance.measure("a-to-b", "a", "b");
log.push(["measure", meas.constructor.name, meas.entryType, meas.duration >= 0, meas.detail]);
const meas2 = performance.measure("opts", { start: "a", end: "b", detail: "d" });
log.push(["measure2", meas2.detail]);
try { performance.measure("bad", "nope"); } catch (e) { log.push([e.name, e.message, e.code]); }
log.push(["byName", performance.getEntriesByName("a").length, performance.getEntriesByType("mark").length, performance.getEntries().length]);
performance.clearMarks("a");
log.push(["cleared", performance.getEntriesByType("mark").map((e) => e.name)]);
const f = performance.timerify(function add(a, b) { return a + b; });
log.push(["timerify", f(1, 2), f.name, f.length]);
const h = createHistogram();
h.record(5); h.record(10); h.record(15);
log.push(["histogram", h.min, h.max, h.mean, h.count, h.percentile(50), h.exceeds, typeof h.stddev]);
const eld = monitorEventLoopDelay({ resolution: 10 });
eld.enable();
const elu = performance.eventLoopUtilization();
log.push(["elu", Object.keys(elu)]);
log.push(["nodeTiming", performance.nodeTiming.constructor.name, Object.keys(performance.nodeTiming.toJSON ? performance.nodeTiming.toJSON() : performance.nodeTiming).sort()]);
log.push(["toJSON", Object.keys(performance.toJSON()).sort()]);
setTimeout(() => {
  eld.disable();
  log.push(["eld", eld.count > 0, eld.min > 0, eld.max >= eld.min, typeof eld.percentile(99)]);
  obs.disconnect();
  console.log(JSON.stringify(log));
}, 60);
"###;
const EXPECTED: &str = r###"[["same",true,"function",["dns","function","gc","http","http2","mark","measure","net","resource"]],["mark","PerformanceMark","a","mark",0,{"x":1},"number",["name","entryType","startTime","duration","detail"]],["measure","PerformanceMeasure","measure",true,null],["measure2","d"],["SyntaxError","The \"nope\" performance mark has not been set",12],["byName",1,2,4],["cleared",["b"]],["timerify",3,"timerified add",2],["histogram",5,15,10,3,10,0,"number"],["elu",["idle","active","utilization"]],["nodeTiming","PerformanceNodeTiming",["bootstrapComplete","duration","entryType","environment","idleTime","loopExit","loopStart","name","nodeStart","startTime","v8Start"]],["toJSON",["eventLoopUtilization","nodeTiming","timeOrigin"]],["observed",[["measure","a-to-b"],["measure","opts"],["mark","b"]],2,true],["eld",true,true,true,"number"]]
"###;
