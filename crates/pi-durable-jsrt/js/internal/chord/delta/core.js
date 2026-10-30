// pi-durable:internal/chord/delta/core — Chord operations over plain JS values.
//
// Validation (op shape, wire grammar, path safety), string overlap and the wire codec are native
// (src/dispatch/chord/delta.rs `delta.*`). Only an op's *shape* crosses: payload slots are
// replaced by placeholders and the original payload objects are put back, so payload ownership
// and identity stay the caller's. The appliers mutate or copy-on-write JS containers in place
// (identity of untouched subtrees and payloads is observable), so they are JS over native
// validation.

import { call, PathError, UnsafePathError } from "pi-durable:bridge";

export { PathError, UnsafePathError };

const isObj = (value) => value !== null && typeof value === "object";

// ─── Shapes ──────────────────────────────────────────────────────────────────

/** A path as native sees it: anything that is not a finite number or a string is unsafe (`-1`). */
function shapePath(path) {
  if (!Array.isArray(path)) return typeof path === "string" || typeof path === "number" ? path : 0;
  const out = new Array(path.length);
  for (let index = 0; index < path.length; index++) {
    const segment = path[index];
    out[index] = typeof segment === "string" || (typeof segment === "number" && Number.isFinite(segment)) ? segment : -1;
  }
  return out;
}

/** A scalar slot: finite numbers, strings, booleans and null pass; anything else is invalid there. */
const shapeScalar = (value) =>
  value === null || typeof value === "string" || typeof value === "boolean" || (typeof value === "number" && Number.isFinite(value))
    ? value
    : -1;

function shapePermutation(value) {
  if (!Array.isArray(value)) return shapeScalar(value);
  return value.map((index) => (typeof index === "number" && Number.isFinite(index) ? index : -1));
}

const shapeItems = (items) => (Array.isArray(items) ? [] : 0);

/** The decoded-op shape: payload slots (`r`/`s` values, `p` items) replaced. */
export function opShape(op) {
  if (!Array.isArray(op) || op.length === 0) return 0;
  const out = new Array(op.length);
  out[0] = shapeScalar(op[0]);
  for (let index = 1; index < op.length; index++) out[index] = op[index];
  switch (op[0]) {
    case "r":
      if (op.length === 2) out[1] = null;
      else for (let index = 1; index < op.length; index++) out[index] = null;
      break;
    case "s":
      out[1] = shapePath(op[1]);
      for (let index = 2; index < op.length; index++) out[index] = null;
      break;
    case "p":
      out[1] = shapePath(op[1]);
      for (let index = 2; index < op.length; index++) out[index] = index === 4 ? shapeItems(op[4]) : shapeScalar(op[index]);
      break;
    case "m":
      out[1] = shapePath(op[1]);
      for (let index = 2; index < op.length; index++) out[index] = index === 2 ? shapePermutation(op[2]) : null;
      break;
    default:
      if (op.length > 1) out[1] = shapePath(op[1]);
      for (let index = 2; index < op.length; index++) out[index] = shapeScalar(op[index]);
  }
  return out;
}

/** The wire-op shape: ids, short forms and `#` definitions are legal here. */
function wireShape(op) {
  if (!Array.isArray(op) || op.length === 0) return 0;
  const verb = op[0];
  const out = op.map((part, index) => (index === 0 ? shapeScalar(part) : part));
  const ref = (index) => {
    out[index] = typeof op[index] === "number" ? shapeScalar(op[index]) : shapePath(op[index]);
  };
  const last = op.length - 1;
  switch (verb) {
    case "r":
      for (let index = 1; index < op.length; index++) out[index] = null;
      break;
    case "s":
      if (op.length === 3) ref(1);
      if (op.length >= 2) out[last] = null;
      break;
    case "p":
      if (op.length === 5) ref(1);
      for (let index = op.length === 5 ? 2 : 1; index < last; index++) out[index] = shapeScalar(op[index]);
      if (op.length >= 4) out[last] = shapeItems(op[last]);
      break;
    case "m":
      if (op.length === 3) ref(1);
      if (op.length >= 2) out[last] = shapePermutation(op[last]);
      break;
    case "#":
      out[1] = shapeScalar(op[1]);
      out[2] = Array.isArray(op[2]) ? shapePath(op[2]) : shapeScalar(op[2]);
      break;
    default:
      if (op.length === 3 || (verb === "d" && op.length === 2)) ref(1);
      for (let index = verb === "d" ? 2 : op.length === 3 ? 2 : 1; index < op.length; index++) out[index] = shapeScalar(op[index]);
  }
  return out;
}

/** Index of the payload slot of a decoded op, or -1. */
const payloadSlot = (verb) => (verb === "r" ? 1 : verb === "s" ? 2 : verb === "p" ? 4 : -1);

// ─── Classification ──────────────────────────────────────────────────────────

export const isReplace = (op) => op[0] === "r";

export const isBase = (ops) => ops.length > 0 && ops[0][0] === "r";

// ─── Overlap ─────────────────────────────────────────────────────────────────

/** Longest suffix of `a` that is a prefix of `b`, within Chord's bounded candidate budget. */
export function overlap(a, b, scan, probe = 64, maxCandidates = 8) {
  return call("delta.overlap", String(a), String(b), scan, probe, maxCandidates);
}

// ─── Path safety ─────────────────────────────────────────────────────────────

export const RESERVED_SEGMENTS = new Set(["__proto__", "constructor", "prototype"]);

export function assertValidOp(op) {
  call("delta.assertValidOp", opShape(op));
}

export function assertValidWireOp(op) {
  call("delta.assertValidWireOp", wireShape(op));
}

export function assertSafePath(path) {
  // Iterates like upstream: any iterable of segments.
  const segments = Array.isArray(path) ? path : Array.from(path);
  call("delta.assertSafePath", shapePath(segments));
}

function assertIndexInRange(parent, index) {
  if (index > parent.length) throw new UnsafePathError(index);
}

// ─── Applier ─────────────────────────────────────────────────────────────────

export function apply(target, ops) {
  return applyOps(target, ops);
}

function applyOps(target, ops) {
  let root = target;
  for (const op of ops) {
    assertValidOp(op);
    if (op[0] === "r") {
      root = op[1];
      continue;
    }
    const path = op[1];
    if (op[0] === "p") {
      const target_ = path.length === 0 ? root : resolve(root, path);
      if (!Array.isArray(target_)) throw new PathError(path);
      target_.splice(op[2], op[3]);
      const chunkSize = 10_000;
      for (let offset = 0; offset < op[4].length; offset += chunkSize) {
        target_.splice(op[2] + offset, 0, ...op[4].slice(offset, offset + chunkSize));
      }
      continue;
    }
    if (op[0] === "m") {
      const target_ = path.length === 0 ? root : resolve(root, path);
      if (!Array.isArray(target_) || target_.length !== op[2].length) throw new PathError(path);
      const previous = target_.slice();
      for (let index = 0; index < op[2].length; index++) target_[index] = previous[op[2][index]];
      continue;
    }
    const parent = resolve(root, path.slice(0, -1));
    const key = path[path.length - 1];
    if (Array.isArray(parent)) {
      if (typeof key !== "number") throw new UnsafePathError(key);
      assertIndexInRange(parent, key);
    }
    const write = (value) => {
      Object.defineProperty(parent, key, { value, writable: true, enumerable: true, configurable: true });
    };
    const read = () => (Object.hasOwn(parent, key) ? parent[key] : undefined);
    switch (op[0]) {
      case "s":
        write(op[2]);
        break;
      case "d":
        if (Array.isArray(parent)) {
          if (typeof key !== "number" || key >= parent.length) throw new PathError(path);
          parent.splice(key, 1);
        } else delete parent[key];
        break;
      case "a": {
        const current = read();
        if (typeof current !== "string") throw new PathError(path);
        write(`${current}${op[2]}`);
        break;
      }
      case "t": {
        const current = read();
        if (typeof current !== "string") throw new PathError(path);
        write(current.slice(op[2]));
        break;
      }
    }
  }
  return root;
}

export function applyImmutable(target, ops) {
  return applyImmutableBatches(target, [ops]);
}

export function applyImmutableBatches(target, batches) {
  let root = target;
  const owned = new WeakSet();
  for (const ops of batches) {
    for (const op of ops) {
      assertValidOp(op);
      if (op[0] === "r") {
        root = op[1];
        continue;
      }
      if (op[0] === "p") {
        const appended = appendCopy(root, op, owned);
        if (appended !== undefined) {
          root = appended;
          continue;
        }
      }
      root = copyContainers(root, op[0] === "p" || op[0] === "m" ? op[1] : op[1].slice(0, -1), owned);
      root = applyOps(root, [op]);
    }
  }
  return root;
}

function appendCopy(root, op, owned) {
  const [, path, index, deleteCount, items] = op;
  if (deleteCount !== 0) return undefined;
  let target = root;
  for (const segment of path) {
    const valid =
      typeof segment === "string"
        ? !RESERVED_SEGMENTS.has(segment) && isObj(target) && !Array.isArray(target)
        : Number.isInteger(segment) && segment >= 0 && Array.isArray(target);
    if (!valid || !Object.hasOwn(target, segment)) return undefined;
    target = target[segment];
  }
  if (!Array.isArray(target) || owned.has(target) || index !== target.length) return undefined;
  const appended = target.concat(items);
  owned.add(appended);
  if (path.length === 0) return appended;
  const parentPath = path.slice(0, -1);
  const copiedRoot = copyContainers(root, parentPath, owned);
  Object.defineProperty(resolve(copiedRoot, parentPath), path[path.length - 1], {
    value: appended,
    writable: true,
    enumerable: true,
    configurable: true,
  });
  return copiedRoot;
}

function copyContainers(root, path, owned) {
  const copy = (value) => {
    if (Array.isArray(value)) return value.slice();
    if (!isObj(value)) throw new PathError(path);
    const result = Object.create(Object.getPrototypeOf(value) === null ? null : Object.prototype);
    for (const key of Object.keys(value)) {
      Object.defineProperty(result, key, { value: value[key], writable: true, enumerable: true, configurable: true });
    }
    return result;
  };
  if (!isObj(root)) throw new PathError(path);
  let copiedRoot;
  if (owned.has(root)) copiedRoot = root;
  else {
    copiedRoot = copy(root);
    owned.add(copiedRoot);
  }
  let destination = copiedRoot;
  for (const segment of path) {
    if (!Object.hasOwn(destination, segment)) throw new PathError(path);
    if (Array.isArray(destination) && typeof segment !== "number") throw new UnsafePathError(segment);
    const child = destination[segment];
    if (!isObj(child)) throw new PathError(path);
    if (owned.has(child)) {
      destination = child;
      continue;
    }
    const copiedChild = copy(child);
    Object.defineProperty(destination, segment, { value: copiedChild, writable: true, enumerable: true, configurable: true });
    owned.add(copiedChild);
    destination = copiedChild;
  }
  return copiedRoot;
}

function resolveValue(root, path) {
  let node = root;
  for (const seg of path) {
    if (!isObj(node)) throw new PathError(path);
    if (Array.isArray(node) && typeof seg !== "number") throw new UnsafePathError(seg);
    if (!Object.hasOwn(node, seg)) throw new PathError(path);
    node = node[seg];
  }
  return node;
}

function resolve(root, path) {
  const node = resolveValue(root, path);
  if (!isObj(node)) throw new PathError(path);
  return node;
}

// ─── Codec (native) ──────────────────────────────────────────────────────────

/** Path interning on second use and arity omission (native `Encoder`). */
export function encoder() {
  const native = call("delta.encoder");
  return {
    encode(ops) {
      const shapes = [];
      for (const op of ops) {
        assertValidOp(op);
        shapes.push(opShape(op));
      }
      const wire = call("delta.encode", native, shapes);
      let next = 0;
      for (const out of wire) {
        if (out[0] === "#") continue;
        const op = ops[next++];
        const slot = payloadSlot(op[0]);
        if (slot >= 0) out[out.length - 1] = op[slot];
      }
      return wire;
    },
  };
}

/** Wire batches back to ops (native `Decoder`); payloads are the received objects. */
export function decoder() {
  const native = call("delta.decoder");
  return {
    decode(wire) {
      const shapes = [];
      for (const op of wire) {
        assertValidWireOp(op);
        shapes.push(wireShape(op));
      }
      const ops = call("delta.decode", native, shapes);
      let next = 0;
      for (const op of ops) {
        while (wire[next][0] === "#") next++;
        const received = wire[next++];
        const slot = payloadSlot(op[0]);
        if (slot >= 0) op[slot] = received[received.length - 1];
      }
      return ops;
    },
  };
}
