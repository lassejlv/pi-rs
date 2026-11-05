// pi-durable:internal/chord/delta/diff — `diffRevisions` over the native diff
// (pi_chord::delta::diff_revisions, `delta.diff`).
//
// Upstream aligns arrays by JS object identity. Both revisions cross as one identity graph (every
// container once, children as node references), so a container shared by reference becomes one
// shared native `Arc`, which is what the native diff aligns on. Payloads come back as node
// references and resolve to the caller's own objects, as upstream's ops alias `after`.

import { call } from "pi-durable:bridge";

function graph(roots) {
  const ids = new Map();
  const objects = [];
  const nodes = [];
  const code = (value) => {
    if (value === null || typeof value !== "object") return value;
    let id = ids.get(value);
    if (id !== undefined) return [id];
    id = nodes.length;
    ids.set(value, id);
    objects.push(value);
    const node = [];
    nodes.push(node);
    if (Array.isArray(value)) {
      node.push(0);
      for (let index = 0; index < value.length; index++) node.push(code(value[index]));
    } else {
      node.push(1);
      for (const key of Object.keys(value)) node.push(key, code(value[key]));
    }
    return [id];
  };
  return { codes: roots.map(code), nodes, objects };
}

/** Compute a compact operation batch from two immutable JSON revisions. */
export function diffRevisions(before, after) {
  const { codes, nodes, objects } = graph([before, after]);
  const value = (encoded) =>
    Array.isArray(encoded) ? objects[encoded[0]] : encoded !== null && typeof encoded === "object" ? encoded.v : encoded;
  const ops = call("delta.diff", nodes, codes[0], codes[1]);
  for (const op of ops) {
    if (op[0] === "r") op[1] = value(op[1]);
    else if (op[0] === "s") op[2] = value(op[2]);
    else if (op[0] === "p") op[4] = op[4].map(value);
  }
  return ops;
}
