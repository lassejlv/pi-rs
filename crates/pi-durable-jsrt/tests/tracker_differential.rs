//! The two tracker engines must not silently diverge: the embedded JS draft (a port of upstream
//! tracker.ts, `@earendil-works/chord/delta` `track`) and the native `pi_chord::delta::Tracker`
//! (`state()` edits, `splice`, `permute`) run the same edit scripts, and wherever the native API
//! can express the edits the op batches must be identical.

use pi_chord::delta::{Op, Seg, track};
use pi_durable_jsrt::{HostOptions, install};
use pi_json::strict::{self, Value};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::json;

/// Copy-on-write edit at `path` inside the native draft (unchanged siblings keep their `Arc`s).
fn at<'a>(root: &'a mut Value, path: &[Seg]) -> &'a mut Value {
    path.iter().fold(root, |node, segment| match segment {
        Seg::Key(key) => node.as_object_mut().unwrap().get_mut(key).unwrap(),
        Seg::Index(index) => &mut node.as_array_mut().unwrap()[*index],
    })
}

fn path(value: &serde_json::Value) -> Vec<Seg> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|s| match s {
            serde_json::Value::String(k) => Seg::from(k.as_str()),
            n => Seg::Index(n.as_u64().unwrap() as usize),
        })
        .collect()
}

fn native_ops(initial: &serde_json::Value, edits: &[serde_json::Value]) -> serde_json::Value {
    let tracker = track(Value::from(initial.clone()));
    let mut change = tracker.begin_change();
    for edit in edits {
        let p = path(&edit["path"]);
        match edit["kind"].as_str().unwrap() {
            "set" => *at(change.state().unwrap(), &p) = Value::from(edit["value"].clone()),
            "delete" => {
                let (key, parent) = p.split_last().unwrap();
                let Seg::Key(key) = key else {
                    panic!("delete takes a key")
                };
                at(change.state().unwrap(), parent)
                    .as_object_mut()
                    .unwrap()
                    .shift_remove(key);
            }
            "append" => {
                let node = at(change.state().unwrap(), &p);
                let mut text = node.as_string().unwrap().clone();
                text.append(&edit["text"].as_str().unwrap().into());
                *node = Value::String(text);
            }
            "splice" => {
                let items = edit["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| Value::from(v.clone()))
                    .collect();
                change
                    .splice(
                        &p,
                        edit["start"].as_u64().unwrap() as usize,
                        edit["remove"].as_u64().unwrap() as usize,
                        items,
                    )
                    .unwrap();
            }
            "permute" => {
                let order: Vec<usize> = edit["order"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as usize)
                    .collect();
                change.permute(&p, &order).unwrap();
            }
            other => panic!("unknown edit {other}"),
        }
    }
    let prepared = change.prepare().unwrap();
    let ops: Vec<Value> = prepared.ops().iter().map(Op::to_value).collect();
    serde_json::from_str(&strict::stringify(&Value::Array(ops.into()))).unwrap()
}

const SCRIPTS: &str = r#"[
  { "name": "set scalars and nested", "initial": { "count": 1, "nested": { "text": "a", "n": 0 } },
    "edits": [ { "kind": "set", "path": ["count"], "value": 2 }, { "kind": "set", "path": ["nested", "n"], "value": 5 } ] },
  { "name": "delete a key", "initial": { "keep": 1, "drop": true },
    "edits": [ { "kind": "delete", "path": ["drop"] } ] },
  { "name": "string append", "initial": { "nested": { "text": "a" } },
    "edits": [ { "kind": "append", "path": ["nested", "text"], "text": "bc" } ] },
  { "name": "append to an array", "initial": { "values": [1, 2, 3] },
    "edits": [ { "kind": "splice", "path": ["values"], "start": 3, "remove": 0, "items": [4] } ] },
  { "name": "removals from the end backwards, then inserts", "initial": { "values": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] },
    "edits": [
      { "kind": "splice", "path": ["values"], "start": 0, "remove": 1, "items": [] },
      { "kind": "splice", "path": ["values"], "start": 1, "remove": 2, "items": [20] },
      { "kind": "splice", "path": ["values"], "start": 5, "remove": 1, "items": [] },
      { "kind": "splice", "path": ["values"], "start": 6, "remove": 1, "items": [] },
      { "kind": "splice", "path": ["values"], "start": 6, "remove": 0, "items": [30] } ] },
  { "name": "insert then remove cancels", "initial": { "values": [1, 2] },
    "edits": [
      { "kind": "splice", "path": ["values"], "start": 1, "remove": 0, "items": [9] },
      { "kind": "splice", "path": ["values"], "start": 1, "remove": 1, "items": [] } ] },
  { "name": "reverse", "initial": { "values": [0, 1, 2, 3, 4] },
    "edits": [ { "kind": "permute", "path": ["values"], "order": [4, 3, 2, 1, 0] } ] },
  { "name": "parity sort", "initial": { "values": [0, 1, 2, 3, 4] },
    "edits": [ { "kind": "permute", "path": ["values"], "order": [0, 2, 4, 1, 3] } ] },
  { "name": "move then edit the moved element", "initial": { "values": [{ "rank": 3, "edited": 0 }, { "rank": 1, "edited": 0 }, { "rank": 2, "edited": 0 }] },
    "edits": [ { "kind": "permute", "path": ["values"], "order": [1, 2, 0] }, { "kind": "set", "path": ["values", 2, "edited"], "value": 1 } ] },
  { "name": "remove, permute, insert", "initial": { "values": [0, 1, 2, 3, 4, 5] },
    "edits": [
      { "kind": "splice", "path": ["values"], "start": 1, "remove": 1, "items": [] },
      { "kind": "permute", "path": ["values"], "order": [4, 0, 1, 2, 3] },
      { "kind": "splice", "path": ["values"], "start": 2, "remove": 0, "items": [7, 8] } ] },
  { "name": "queue shift/push", "initial": { "values": [{ "v": 0 }, { "v": 1 }, { "v": 2 }, { "v": 3 }] },
    "edits": [
      { "kind": "splice", "path": ["values"], "start": 0, "remove": 1, "items": [] },
      { "kind": "splice", "path": ["values"], "start": 3, "remove": 0, "items": [{ "v": 4 }] },
      { "kind": "splice", "path": ["values"], "start": 0, "remove": 1, "items": [] },
      { "kind": "splice", "path": ["values"], "start": 3, "remove": 0, "items": [{ "v": 5 }] } ] }
]"#;

const JS_RUNNER: &str = r#"
import { track } from "@earendil-works/chord/delta";
const walk = (draft, path) => path.reduce((node, segment) => node[segment], draft);
export function run(scripts) {
  return scripts.map(({ initial, edits }) => {
    const change = track(structuredClone(initial)).beginChange();
    for (const edit of edits) {
      const parent = walk(change.state, edit.path.slice(0, -1));
      const key = edit.path.at(-1);
      const target = walk(change.state, edit.path);
      switch (edit.kind) {
        case "set": parent[key] = edit.value; break;
        case "delete": delete parent[key]; break;
        case "append": parent[key] += edit.text; break;
        case "splice": target.splice(edit.start, edit.remove, ...edit.items); break;
        case "permute": {
          // The draft's own mutators: `reverse`, or `sort` with a comparator ranking each element
          // by its target position (elements are distinct values or proxies here).
          if (edit.order.every((source, index) => source === edit.order.length - 1 - index)) target.reverse();
          else {
            const rank = new Map();
            edit.order.forEach((source, index) => rank.set(target[source], index));
            target.sort((left, right) => rank.get(left) - rank.get(right));
          }
          break;
        }
      }
    }
    return change.prepare().ops;
  });
}
"#;

#[tokio::test]
async fn js_and_native_trackers_emit_identical_batches() {
    let scripts: Vec<serde_json::Value> = serde_json::from_str(SCRIPTS).unwrap();
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    install(&rt, HostOptions::default()).await.unwrap();
    let ns = rt
        .eval_module(JS_RUNNER, "/tracker-differential.mjs")
        .await
        .unwrap();
    let js = ns.call_method("run", vec![json!(scripts)]).await.unwrap();
    let mut mismatches = Vec::new();
    for (index, script) in scripts.iter().enumerate() {
        let native = native_ops(&script["initial"], script["edits"].as_array().unwrap());
        if js[index] != native {
            mismatches.push(json!({ "script": script["name"], "js": js[index], "native": native }));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{}",
        serde_json::to_string_pretty(&mismatches).unwrap()
    );
    // Not vacuous: the scripts exercise every op kind the native API can produce.
    let verbs: std::collections::BTreeSet<String> = js
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|ops| {
            ops.as_array()
                .unwrap()
                .iter()
                .map(|op| op[0].as_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(
        verbs.into_iter().collect::<Vec<_>>(),
        ["a", "d", "m", "p", "s"]
    );
    assert_eq!(js[4].as_array().unwrap().len(), 6, "{}", js[4]);
    assert_eq!(js[5], json!([]));
}
