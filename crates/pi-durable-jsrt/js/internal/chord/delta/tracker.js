// pi-durable:internal/chord/delta/tracker — `track`: the Proxy draft over immutable JS revisions.
//
// A port of the upstream overlay tracker (packages/chord/src/delta/tracker.ts at 5e63241, types
// stripped). The draft is a JS object graph whose identity, proxies, held handles, mutator
// coercion order and GC retention are observable, and whose operation payloads alias the
// materialized revision, so the overlay and its emission stay JS. Its native touchpoints: every
// placement is copied by `copyJson` (native `json.copy` after the structural check), string
// edits use the native `overlap`, and materialization goes through `applyImmutable`, whose ops
// are validated natively. Rust code uses the native engine, `pi_chord::delta::Tracker`.

import { copyJson } from "pi-durable:internal/chord/json";
import { applyImmutableTrusted } from "pi-durable:internal/chord/delta/trusted";
import { applyImmutable, overlap, RESERVED_SEGMENTS } from "pi-durable:internal/chord/delta/core";

const NODE = Symbol("chord.delta.overlay.node");
const PREPARED = new WeakMap();
const RELEASED = {};
const ARRAY_MUTATORS = new Set([
  "push",
  "pop",
  "shift",
  "unshift",
  "splice",
  "reverse",
  "sort",
  "fill",
  "copyWithin"
]);
const MAX_DELTA_OPERATIONS = 4096;
const MAX_SIMPLE_OBJECT_NODES = 128;
const DATA_DESCRIPTOR = {
  value: void 0,
  writable: true,
  enumerable: true,
  configurable: true
};
class PreparedImpl {
  #context;
  value;
  base;
  ops;
  constructor(context, value, operations) {
    this.#context = context;
    this.value = value;
    this.base = context.baseValue;
    this.ops = operations;
    PREPARED.set(this, context);
  }
  get baseRevision() {
    return this.#context.baseRevision;
  }
  abort() {
    abortContext(this.#context);
  }
}
class ChangeImpl {
  #context;
  #preparedStatus;
  #settled = false;
  constructor(context) {
    this.#context = context;
  }
  get state() {
    const state = this.#context?.root?.proxy;
    if (state === void 0) throw new TypeError("Cannot use a settled overlay");
    return state;
  }
  prepare() {
    if (this.#settled) throw new Error("Change has already been settled");
    const context = this.#context;
    assertWritable(context);
    context.status.value = "prepared";
    try {
      if (!context.replacement) context.ops = context.dirty.length === 0 ? [] : emitOperations(context);
      const prepared = materializePrepared(context);
      this.#preparedStatus = context.status;
      this.#settled = true;
      return prepared;
    } catch (error) {
      context.status.value = "aborted";
      clearContext(context);
      this.#settled = true;
      throw error;
    } finally {
      this.#context = void 0;
    }
  }
  abort() {
    if (this.#settled) {
      if (this.#preparedStatus?.value === "prepared") this.#preparedStatus.value = "aborted";
      this.#preparedStatus = void 0;
      return;
    }
    this.#settled = true;
    abortContext(this.#context);
    this.#context = void 0;
  }
}
class TrackerImpl {
  #owner = {};
  #contexts = new Set();
  #selfRef;
  #value;
  #revision = 0;
  #pruneBudget = 256;
  constructor(initial) {
    this.#value = initial;
    this.#selfRef = new WeakRef(this);
  }
  get value() {
    return this.#value;
  }
  get revision() {
    return this.#revision;
  }
  beginChange() {
    const context = createContext(this.#selfRef, this.#owner, this.#revision, this.#value, false, this.#value);
    this.#register(context);
    return new ChangeImpl(context);
  }
  prepareReplace(value) {
    const context = createContext(this.#selfRef, this.#owner, this.#revision, value, true, this.#value);
    context.status.value = "prepared";
    this.#register(context);
    try {
      return materializePrepared(context);
    } catch (error) {
      context.status.value = "aborted";
      clearContext(context);
      throw error;
    }
  }
  adopt(prepared) {
    const context = PREPARED.get(prepared);
    if (context?.owner !== this.#owner) throw new Error("Prepared change belongs to a different tracker");
    if (context.status.value === "consumed") throw new Error("Prepared change has already been used");
    if (context.status.value === "aborted") throw new Error("Prepared change has been aborted");
    if (context.status.value === "stale") throw new Error("Prepared change is stale");
    if (context.status.value !== "prepared") throw new Error("Prepared change is not ready");
    if (context.baseRevision !== this.#revision) {
      context.status.value = "stale";
      clearContext(context);
      throw new Error("Prepared change is stale");
    }
    if (this.#value !== prepared.base) {
      context.status.value = "stale";
      throw new Error("Prepared change is stale");
    }
    this.#value = prepared.value;
    context.status.value = "consumed";
    this.#revision += 1;
    this.#invalidate(context);
  }
  releaseContext(context) {
    if (context.registryRef !== void 0) this.#contexts.delete(context.registryRef);
    context.registryRef = void 0;
  }
  #register(context) {
    const reference = new WeakRef(context);
    context.registryRef = reference;
    this.#contexts.add(reference);
    this.#pruneBudget -= 1;
    if (this.#pruneBudget === 0) {
      this.#prune();
      this.#pruneBudget = Math.max(256, this.#contexts.size);
    }
  }
  #prune() {
    for (const reference of this.#contexts) {
      if (reference.deref() === void 0) this.#contexts.delete(reference);
    }
  }
  #invalidate(winner) {
    for (const reference of this.#contexts) {
      const context = reference.deref();
      if (context === void 0 || context === winner) continue;
      if (context.status.value === "open" || context.status.value === "prepared") context.status.value = "stale";
      releaseOverlayReferences(context);
    }
    this.#contexts.clear();
    this.#pruneBudget = 256;
  }
  replacementBase(context) {
    return context.baseRevision === this.#revision ? this.#value : void 0;
  }
}
function track(initial) {
  return new TrackerImpl(initial);
}
function materializePrepared(context) {
  const operations = ensureOperations(context);
  const base = context.baseValue;
  const value = operations.length === 0 ? base : context.simpleObjectMaterialization ? cloneNode(context.root) : materializeOperations(base, operations);
  const prepared = new PreparedImpl(context, value, operations);
  if (context.nodes.length > 4096) releaseOverlayReferences(context);
  else clearContext(context);
  return prepared;
}
function materializeOperations(base, operations) {
  for (const operation of operations) {
    if (operation[0] === "p" || operation[0] === "m") return applyImmutable(base, operations);
  }
  return applyImmutableTrusted(base, operations);
}
function createContext(tracker, owner, baseRevision, root, replacement, base) {
  const context = {
    owner,
    tracker,
    baseRevision,
    status: { value: "open" },
    root: void 0,
    bases: [],
    stored: [],
    dirty: [],
    nodes: [],
    rawNodes: new WeakMap(),
    ops: void 0,
    replacement,
    replacementNoop: false,
    baseValue: base,
    overlayReleased: false,
    simpleObjectMaterialization: false,
    registryRef: void 0
  };
  context.root = createNode(context, root, void 0);
  return context;
}
const sharedObjectHandler = {
  deleteProperty(target, property) {
    return deleteProperty(nodeForTarget(target), property);
  },
  defineProperty(target) {
    assertWritable(nodeForTarget(target).context);
    throw new TypeError("Defining overlay properties is not supported");
  },
  get(target, property) {
    if (property === "then") {
      const node2 = target[NODE];
      if (node2 === void 0 || isSettledContext(node2.context)) return void 0;
    }
    const node = nodeForTarget(target);
    if (property === NODE) return node;
    return getProperty(node, property);
  },
  getOwnPropertyDescriptor(target, property) {
    return getDescriptor(nodeForTarget(target), property);
  },
  getPrototypeOf(target) {
    const node = nodeForTarget(target);
    assertReadable(node.context);
    return Object.getPrototypeOf(nodeBase(node));
  },
  has(target, property) {
    return hasProperty(nodeForTarget(target), property);
  },
  isExtensible(target) {
    assertReadable(nodeForTarget(target).context);
    return true;
  },
  ownKeys(target) {
    return ownKeys(nodeForTarget(target));
  },
  preventExtensions(target) {
    assertWritable(nodeForTarget(target).context);
    throw new TypeError("Overlays cannot be made non-extensible");
  },
  set(target, property, value) {
    return setProperty(nodeForTarget(target), property, value);
  },
  setPrototypeOf(target) {
    assertWritable(nodeForTarget(target).context);
    throw new TypeError("Changing an overlay prototype is not supported");
  }
};
const sharedArrayHandler = sharedObjectHandler;
function createNode(context, base, parent, parentKind = 0, parentKey = "", parentSource = void 0, parentPlacement = false) {
  const existing = context.rawNodes.get(base);
  if (existing !== void 0) return existing;
  const target = Array.isArray(base) ? [] : {};
  const baseIndex = context.bases.length;
  const parentIndex = parent?.baseIndex ?? -1;
  context.bases.push(base);
  const node = {
    context,
    baseIndex,
    parentIndex,
    parentKind,
    parentKey,
    parentPlacement,
    target,
    proxy: target
  };
  if (parentSource !== void 0) node.parentSource = parentSource;
  target[NODE] = node;
  node.proxy = new Proxy(target, Array.isArray(base) ? sharedArrayHandler : sharedObjectHandler);
  context.rawNodes.set(base, node);
  context.nodes.push(node);
  return node;
}
function nodeBase(node) {
  return node.context.bases[node.baseIndex];
}
function nodeParent(node) {
  return node.parentIndex < 0 ? void 0 : node.context.nodes[node.parentIndex];
}
function storeValue(context, value) {
  if (!isContainer(value)) return value;
  const index = context.stored.length;
  context.stored.push(value);
  return { index };
}
function storedValue(context, reference) {
  return typeof reference === "object" && reference !== null ? context.stored[reference.index] : reference;
}
function replaceStoredValue(context, reference, value) {
  if (isContainer(value)) {
    if (typeof reference === "object" && reference !== null) {
      context.stored[reference.index] = value;
      return reference;
    }
    return storeValue(context, value);
  }
  if (typeof reference === "object" && reference !== null) context.stored[reference.index] = RELEASED;
  return value;
}
function releaseStoredValue(context, reference) {
  if (typeof reference === "object" && reference !== null) context.stored[reference.index] = RELEASED;
}
function nodeForTarget(target) {
  const node = target[NODE];
  if (node === void 0) throw new TypeError("Cannot use a settled overlay");
  return node;
}
function isSettledContext(context) {
  return context.overlayReleased || context.status.value === "consumed" || context.status.value === "aborted" || context.status.value === "stale";
}
function assertReadable(context) {
  if (isSettledContext(context)) throw new TypeError("Cannot use a settled overlay");
}
function assertWritable(context) {
  assertReadable(context);
  if (context.status.value !== "open") throw new TypeError("Prepared overlays are read-only");
}
function getProperty(node, property) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    const overlay = arrayOverlay(node);
    if (property === "length") return arrayLength(overlay);
    if (ARRAY_MUTATORS.has(property)) return arrayMutators[property];
    const index = arrayIndex(property);
    if (index !== void 0) return getArrayIndex(node, index);
    return Reflect.get(Array.prototype, property, node.proxy);
  }
  if (typeof property === "symbol") return Reflect.get(nodeBase(node), property, node.proxy);
  const key = String(property);
  if (!objectHas(node, key)) {
    if (isObjectDeleted(node, key) || Object.hasOwn(nodeBase(node), key)) return void 0;
    return Reflect.get(nodeBase(node), property, node.proxy);
  }
  const value = objectValue(node, key);
  if (!isContainer(value)) return value;
  return createNode(node.context, value, node, 0, key, void 0, hasObjectWrite(node, key)).proxy;
}
function getArrayIndex(node, index) {
  const overlay = arrayOverlay(node);
  if (index >= arrayLength(overlay)) return void 0;
  const piece = locatePiece(overlay, index);
  const sourceIndex = piece.start + piece.step * overlay.locatedOffset;
  const value = entryValueAt(node, piece, sourceIndex);
  if (!isContainer(value)) return value;
  return createNode(
    node.context,
    value,
    node,
    piece.kind === "base" ? 1 : 2,
    sourceIndex,
    piece.kind === "insert" ? piece.source : void 0,
    piece.kind === "insert" || hasEntryOverrideAt(overlay, piece, sourceIndex)
  ).proxy;
}
function setProperty(node, property, supplied) {
  assertWritable(node.context);
  if (typeof property === "symbol") throw new TypeError("Symbol writes are not supported");
  if (Array.isArray(nodeBase(node))) {
    if (property === "length") {
      setArrayLength(node, toArrayLength(supplied));
      return true;
    }
    const index = arrayIndex(property);
    if (index === void 0) throw new TypeError("Only array indices and length can be written");
    if (index > arrayLength(arrayOverlay(node))) throw new TypeError("Overlay arrays cannot contain holes");
    setArrayIndex(node, index, clonePlacement(supplied));
    return true;
  }
  const key = String(property);
  if (supplied === void 0) return deleteProperty(node, key);
  const stored = clonePlacement(supplied);
  const current = objectValue(node, key);
  const wasDeleted = isObjectDeleted(node, key);
  if (!wasDeleted && !isContainer(stored) && current === stored) return true;
  setObjectWrite(node, key, stored);
  if (wasDeleted && Object.hasOwn(nodeBase(node), key)) {
    if (node.readded === void 0) node.readded = new Set();
    node.readded.add(key);
  }
  deleteObjectDeletion(node, key);
  markDirty(node);
  return true;
}
function deleteProperty(node, property) {
  assertWritable(node.context);
  if (typeof property === "symbol") throw new TypeError("Symbol writes are not supported");
  if (Array.isArray(nodeBase(node))) throw new TypeError("Overlay arrays cannot contain holes");
  const key = String(property);
  if (!objectHas(node, key)) return true;
  deleteObjectWrite(node, key);
  node.readded?.delete(key);
  setObjectDeletion(node, key);
  markDirty(node);
  return true;
}
function hasProperty(node, property) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    if (property === "length") return true;
    const index = arrayIndex(property);
    if (index !== void 0) return index < arrayLength(arrayOverlay(node));
    return property in Array.prototype;
  }
  return typeof property === "symbol" ? property in nodeBase(node) : objectHas(node, String(property));
}
function ownKeys(node) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    const length = arrayLength(arrayOverlay(node));
    const keys2 = new Array(length + 1);
    for (let index = 0; index < length; index++) keys2[index] = String(index);
    keys2[length] = "length";
    return keys2;
  }
  let existingKeysOnly = node.deleteKey === void 0 && node.deletes === void 0 && node.readded === void 0 && (node.writeKey === void 0 || Object.hasOwn(nodeBase(node), node.writeKey));
  if (existingKeysOnly && node.writes !== void 0) {
    for (const key of node.writes.keys()) {
      if (!Object.hasOwn(nodeBase(node), key)) {
        existingKeysOnly = false;
        break;
      }
    }
  }
  if (existingKeysOnly) return Object.keys(nodeBase(node));
  const keys = Object.keys(nodeBase(node)).filter((key) => !isObjectDeleted(node, key) && !node.readded?.has(key));
  const seen = new Set(keys);
  if (node.writeKey !== void 0 && !seen.has(node.writeKey)) {
    keys.push(node.writeKey);
    seen.add(node.writeKey);
  }
  if (node.writes !== void 0) {
    for (const key of node.writes.keys()) {
      if (seen.has(key)) continue;
      keys.push(key);
      seen.add(key);
    }
  }
  const indices = [];
  const strings = [];
  for (const key of keys) {
    const index = arrayIndex(key);
    if (index === void 0) strings.push(key);
    else indices.push(index);
  }
  indices.sort((left, right) => left - right);
  return [...indices.map(String), ...strings];
}
function getDescriptor(node, property) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    if (property === "length") {
      const length = arrayLength(arrayOverlay(node));
      node.target.length = length;
      return Reflect.getOwnPropertyDescriptor(node.target, "length");
    }
    const index = arrayIndex(property);
    if (index === void 0 || index >= arrayLength(arrayOverlay(node))) return void 0;
  } else {
    if (typeof property === "symbol" || !objectHas(node, String(property))) return void 0;
  }
  return {
    configurable: true,
    enumerable: true,
    writable: node.context.status.value === "open",
    value: getProperty(node, property)
  };
}
function hasObjectWrite(node, key) {
  return node.writeKey === key || (node.writes?.has(key) ?? false);
}
function setObjectWrite(node, key, value) {
  if (node.writes !== void 0) {
    const reference = node.writes.get(key);
    node.writes.set(
      key,
      reference === void 0 ? storeValue(node.context, value) : replaceStoredValue(node.context, reference, value)
    );
    return;
  }
  if (node.writeKey === void 0 || node.writeKey === key) {
    node.writeKey = key;
    node.writeValue = node.writeValue === void 0 ? storeValue(node.context, value) : replaceStoredValue(node.context, node.writeValue, value);
    return;
  }
  node.writes = new Map();
  node.writes.set(node.writeKey, node.writeValue);
  node.writes.set(key, storeValue(node.context, value));
  node.writeKey = void 0;
  node.writeValue = void 0;
}
function deleteObjectWrite(node, key) {
  if (node.writeKey === key) {
    node.writeKey = void 0;
    releaseStoredValue(node.context, node.writeValue);
    node.writeValue = void 0;
  } else {
    const reference = node.writes?.get(key);
    if (reference !== void 0) releaseStoredValue(node.context, reference);
    node.writes?.delete(key);
  }
}
function isObjectDeleted(node, key) {
  return node.deleteKey === key || (node.deletes?.has(key) ?? false);
}
function setObjectDeletion(node, key) {
  if (node.deletes !== void 0) {
    node.deletes.add(key);
    return;
  }
  if (node.deleteKey === void 0 || node.deleteKey === key) {
    node.deleteKey = key;
    return;
  }
  node.deletes = new Set();
  node.deletes.add(node.deleteKey);
  node.deletes.add(key);
  node.deleteKey = void 0;
}
function deleteObjectDeletion(node, key) {
  if (node.deleteKey === key) node.deleteKey = void 0;
  else node.deletes?.delete(key);
}
function objectHas(node, key) {
  if (isObjectDeleted(node, key)) return false;
  return hasObjectWrite(node, key) || Object.hasOwn(nodeBase(node), key);
}
function objectValue(node, key) {
  if (node.writeKey === key) return storedValue(node.context, node.writeValue);
  const reference = node.writes?.get(key);
  if (reference !== void 0) return storedValue(node.context, reference);
  return nodeBase(node)[key];
}
function markDirty(node) {
  if (node.dirty) return;
  node.dirty = true;
  node.context.dirty.push(node);
  for (let parent = nodeParent(node); parent !== void 0; parent = nodeParent(parent)) parent.subtreeDirty = true;
}
function arrayOverlay(node) {
  if (node.array !== void 0) return node.array;
  const base = nodeBase(node);
  const overlay = {
    root: void 0,
    pieces: void 0,
    baseOverrides: void 0,
    insertOverrides: void 0,
    structural: false,
    generation: 0,
    plan: void 0,
    seed: 2654435769,
    locatedOffset: 0,
    baseLocations: void 0,
    insertLocations: void 0
  };
  if (base.length > 0)
    overlay.root = createPieceNode(overlay, { kind: "base", start: 0, length: base.length, step: 1 });
  node.array = overlay;
  return overlay;
}
function baseOverridesForWrite(overlay) {
  if (overlay.baseOverrides === void 0) overlay.baseOverrides = new Map();
  return overlay.baseOverrides;
}
function insertOverridesForWrite(overlay) {
  if (overlay.insertOverrides === void 0) overlay.insertOverrides = new Map();
  return overlay.insertOverrides;
}
function nextPiecePriority(overlay) {
  let value = overlay.seed;
  value ^= value << 13;
  value ^= value >>> 17;
  value ^= value << 5;
  overlay.seed = value >>> 0;
  return overlay.seed;
}
function treeElements(node) {
  return node?.elements ?? 0;
}
function updatePieceNode(node) {
  node.elements = treeElements(node.left) + node.piece.length + treeElements(node.right);
}
function createPieceNode(overlay, piece) {
  return { piece, left: void 0, right: void 0, priority: nextPiecePriority(overlay), elements: piece.length };
}
function mergePieceTrees(left, right) {
  if (left === void 0) return right;
  if (right === void 0) return left;
  if (left.priority >= right.priority) {
    left.right = mergePieceTrees(left.right, right);
    updatePieceNode(left);
    return left;
  }
  right.left = mergePieceTrees(left, right.left);
  updatePieceNode(right);
  return right;
}
function splitPieceTree(overlay, root, index) {
  if (root === void 0) return [void 0, void 0];
  const leftLength = treeElements(root.left);
  if (index < leftLength) {
    const [left, right] = splitPieceTree(overlay, root.left, index);
    root.left = right;
    updatePieceNode(root);
    return [left, root];
  }
  const pieceEnd = leftLength + root.piece.length;
  if (index > pieceEnd) {
    const [left, right] = splitPieceTree(overlay, root.right, index - pieceEnd);
    root.right = left;
    updatePieceNode(root);
    return [root, right];
  }
  if (index === leftLength) {
    const left = root.left;
    root.left = void 0;
    updatePieceNode(root);
    return [left, root];
  }
  if (index === pieceEnd) {
    const right = root.right;
    root.right = void 0;
    updatePieceNode(root);
    return [root, right];
  }
  const offset = index - leftLength;
  const first = { ...root.piece, length: offset };
  const second = {
    ...root.piece,
    start: root.piece.start + root.piece.step * offset,
    length: root.piece.length - offset
  };
  return [
    mergePieceTrees(root.left, createPieceNode(overlay, first)),
    mergePieceTrees(createPieceNode(overlay, second), root.right)
  ];
}
function leftmostPieceNode(node) {
  while (node.left !== void 0) node = node.left;
  return node;
}
function rightmostPieceNode(node) {
  while (node.right !== void 0) node = node.right;
  return node;
}
function mergeablePieces(left, right) {
  if (left.kind !== right.kind) return false;
  if (left.kind === "insert" && left.source !== right.source) return false;
  if (left.length === 1 && right.length === 1) return Math.abs(right.start - left.start) === 1;
  return left.step === right.step && left.start + left.step * left.length === right.start;
}
function joinNormalized(overlay, left, right) {
  if (left === void 0) return right;
  if (right === void 0) return left;
  const leftPiece = rightmostPieceNode(left).piece;
  const rightPiece = leftmostPieceNode(right).piece;
  if (!mergeablePieces(leftPiece, rightPiece)) return mergePieceTrees(left, right);
  const [leftRest] = splitPieceTree(overlay, left, treeElements(left) - leftPiece.length);
  const [, rightRest] = splitPieceTree(overlay, right, rightPiece.length);
  const step = leftPiece.length === 1 ? rightPiece.start - leftPiece.start : leftPiece.step;
  const combined = leftPiece.kind === "base" ? { kind: "base", start: leftPiece.start, length: leftPiece.length + rightPiece.length, step } : {
    kind: "insert",
    source: leftPiece.source,
    start: leftPiece.start,
    length: leftPiece.length + rightPiece.length,
    step
  };
  return joinNormalized(overlay, joinNormalized(overlay, leftRest, createPieceNode(overlay, combined)), rightRest);
}
function flattenPieceTree(node, output) {
  if (node === void 0) return;
  flattenPieceTree(node.left, output);
  output.push(node.piece);
  flattenPieceTree(node.right, output);
}
function piecesOf(overlay) {
  if (overlay.pieces === void 0) {
    overlay.pieces = [];
    flattenPieceTree(overlay.root, overlay.pieces);
  }
  return overlay.pieces;
}
function treeFromPieces(overlay, pieces) {
  mergePieces(pieces);
  let root;
  for (const piece of pieces) root = mergePieceTrees(root, createPieceNode(overlay, piece));
  return root;
}
function replaceAllPieces(overlay, pieces) {
  overlay.root = treeFromPieces(overlay, pieces);
  overlay.pieces = void 0;
  overlay.baseLocations = void 0;
  overlay.insertLocations = void 0;
}
function arrayLength(overlay) {
  return treeElements(overlay.root);
}
function locatePiece(overlay, index) {
  let node = overlay.root;
  while (node !== void 0) {
    const leftLength = treeElements(node.left);
    if (index < leftLength) node = node.left;
    else if (index >= leftLength + node.piece.length) {
      index -= leftLength + node.piece.length;
      node = node.right;
    } else {
      overlay.locatedOffset = index - leftLength;
      return node.piece;
    }
  }
  throw new RangeError("Array overlay index is out of range");
}
function arrayIndex(property) {
  if (typeof property !== "string" || property.length === 0 || property.length > 10) return void 0;
  if (property === "0") return 0;
  const first = property.charCodeAt(0);
  if (first < 49 || first > 57) return void 0;
  let index = first - 48;
  for (let offset = 1; offset < property.length; offset++) {
    const digit = property.charCodeAt(offset) - 48;
    if (digit < 0 || digit > 9) return void 0;
    index = index * 10 + digit;
    if (index >= 4294967295) return void 0;
  }
  return index;
}
function entryValueAt(node, piece, sourceIndex) {
  const overlay = arrayOverlay(node);
  if (piece.kind === "base") {
    const valueIndex = overlay.baseOverrides?.get(sourceIndex);
    return valueIndex === void 0 ? nodeBase(node)[sourceIndex] : storedValue(node.context, valueIndex);
  }
  const overrides = overlay.insertOverrides?.get(piece.source);
  const reference = overrides?.has(sourceIndex) ? overrides.get(sourceIndex) : piece.source.refs[sourceIndex];
  return storedValue(node.context, reference);
}
function hasEntryOverrideAt(overlay, piece, sourceIndex) {
  return piece.kind === "base" ? overlay.baseOverrides?.has(sourceIndex) ?? false : overlay.insertOverrides?.get(piece.source)?.has(sourceIndex) ?? false;
}
function extendRightmostPiece(node, amount) {
  if (node.right !== void 0) extendRightmostPiece(node.right, amount);
  else node.piece.length += amount;
  updatePieceNode(node);
}
function invalidatePieceCaches(overlay) {
  overlay.pieces = void 0;
  overlay.baseLocations = void 0;
  overlay.insertLocations = void 0;
  overlay.plan = void 0;
}
function replacePieceRange(node, index, remove, inserted) {
  if (remove === 0 && inserted.length === 0) return;
  const overlay = arrayOverlay(node);
  if (remove === 0 && index === arrayLength(overlay) && inserted.length === 1 && overlay.root !== void 0) {
    const addition = inserted[0];
    const tail = rightmostPieceNode(overlay.root).piece;
    if (addition.kind === "insert" && tail.kind === "insert" && tail.step === 1 && tail.start + tail.length === tail.source.refs.length) {
      for (let offset = 0; offset < addition.length; offset++) {
        tail.source.refs.push(addition.source.refs[addition.start + offset]);
      }
      extendRightmostPiece(overlay.root, addition.length);
      invalidatePieceCaches(overlay);
      overlay.structural = true;
      overlay.generation += 1;
      markDirty(node);
      return;
    }
  }
  const [left, rest] = splitPieceTree(overlay, overlay.root, index);
  const [, right] = splitPieceTree(overlay, rest, remove);
  const middle = treeFromPieces(overlay, inserted);
  overlay.root = joinNormalized(overlay, joinNormalized(overlay, left, middle), right);
  invalidatePieceCaches(overlay);
  overlay.structural = true;
  overlay.generation += 1;
  markDirty(node);
}
function mergePieces(pieces) {
  for (let index = 1; index < pieces.length; ) {
    const left = pieces[index - 1];
    const right = pieces[index];
    const sameSource = left.kind === right.kind && (left.kind === "base" || left.source === right.source);
    if (sameSource && left.length === 1 && right.length === 1 && Math.abs(right.start - left.start) === 1) {
      left.step = right.start - left.start;
      left.length = 2;
      pieces.splice(index, 1);
    } else if (sameSource && left.step === right.step && left.start + left.step * left.length === right.start) {
      left.length += right.length;
      pieces.splice(index, 1);
    } else index += 1;
  }
}
function insertPiece(node, items, start = 0) {
  if (items.length === start) return [];
  const refs = [];
  for (let index = start; index < items.length; index++) refs.push(storeValue(node.context, items[index]));
  return [{ kind: "insert", source: { refs }, start: 0, length: refs.length, step: 1 }];
}
function insertPlacementPiece(node, values, start = 0) {
  if (values.length === start) return [];
  const storedLength = node.context.stored.length;
  try {
    for (let index = start; index < values.length; index++)
      values[index] = storeValue(node.context, clonePlacement(values[index]));
  } catch (error) {
    node.context.stored.length = storedLength;
    throw error;
  }
  return [
    {
      kind: "insert",
      source: { refs: values },
      start,
      length: values.length - start,
      step: 1
    }
  ];
}
function setArrayIndex(node, index, stored) {
  const overlay = arrayOverlay(node);
  const length = arrayLength(overlay);
  if (index === length) {
    replacePieceRange(node, length, 0, insertPiece(node, [stored]));
    return;
  }
  const piece = locatePiece(overlay, index);
  const sourceIndex = piece.start + piece.step * overlay.locatedOffset;
  const current = entryValueAt(node, piece, sourceIndex);
  if (!isContainer(stored) && current === stored) return;
  if (piece.kind === "base") {
    const existingIndex = overlay.baseOverrides?.get(sourceIndex);
    if (!isContainer(stored) && stored === nodeBase(node)[sourceIndex]) {
      if (existingIndex !== void 0) releaseStoredValue(node.context, existingIndex);
      overlay.baseOverrides?.delete(sourceIndex);
    } else if (existingIndex === void 0) {
      baseOverridesForWrite(overlay).set(sourceIndex, storeValue(node.context, stored));
    } else overlay.baseOverrides.set(sourceIndex, replaceStoredValue(node.context, existingIndex, stored));
  } else {
    let overrides = overlay.insertOverrides?.get(piece.source);
    const existingIndex = overrides?.get(sourceIndex);
    if (!isContainer(stored) && stored === storedValue(node.context, piece.source.refs[sourceIndex])) {
      if (existingIndex !== void 0) releaseStoredValue(node.context, existingIndex);
      overrides?.delete(sourceIndex);
      if (overrides?.size === 0) overlay.insertOverrides?.delete(piece.source);
    } else {
      if (overrides === void 0) {
        overrides = new Map();
        insertOverridesForWrite(overlay).set(piece.source, overrides);
      }
      if (existingIndex === void 0) overrides.set(sourceIndex, storeValue(node.context, stored));
      else overrides.set(sourceIndex, replaceStoredValue(node.context, existingIndex, stored));
    }
  }
  markDirty(node);
}
function setArrayLength(node, next) {
  const current = arrayLength(arrayOverlay(node));
  if (next === current) return;
  if (next < current) replacePieceRange(node, next, current - next, []);
  else
    replacePieceRange(
      node,
      current,
      0,
      insertPiece(
        node,
        Array.from({ length: next - current }, () => null)
      )
    );
}
function toArrayLength(value) {
  const number = Number(value);
  if (!Number.isInteger(number) || number < 0 || number >= 4294967296) throw new RangeError("Invalid array length");
  return number;
}
function toIntegerOrInfinity(value) {
  const number = Number(value);
  if (Number.isNaN(number) || number === 0) return 0;
  return Number.isFinite(number) ? Math.trunc(number) : number;
}
function clampIndex(value, length) {
  if (value === Number.NEGATIVE_INFINITY) return 0;
  if (value < 0) return Math.max(length + value, 0);
  return Math.min(value, length);
}
function mutatorNode(receiver) {
  if (!isContainer(receiver)) return void 0;
  const node = Reflect.get(receiver, NODE);
  if (node === void 0) return void 0;
  if (!Array.isArray(nodeBase(node))) throw new TypeError("Array mutator called on incompatible receiver");
  assertWritable(node.context);
  return node;
}
const arrayMutators = {
  push(...items) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.push, this, items);
    const length = arrayLength(arrayOverlay(node));
    replacePieceRange(node, length, 0, insertPlacementPiece(node, items));
    return length + items.length;
  },
  pop() {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.pop, this, []);
    const length = arrayLength(arrayOverlay(node));
    if (length === 0) return void 0;
    const value = getArrayIndex(node, length - 1);
    replacePieceRange(node, length - 1, 1, []);
    return value;
  },
  shift() {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.shift, this, []);
    const length = arrayLength(arrayOverlay(node));
    if (length === 0) return void 0;
    const value = getArrayIndex(node, 0);
    replacePieceRange(node, 0, 1, []);
    return value;
  },
  unshift(...items) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.unshift, this, items);
    replacePieceRange(node, 0, 0, insertPlacementPiece(node, items));
    return arrayLength(arrayOverlay(node));
  },
  splice(...args) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.splice, this, args);
    const length = arrayLength(arrayOverlay(node));
    const start = args.length === 0 ? 0 : clampIndex(toIntegerOrInfinity(args[0]), length);
    const remove = args.length === 0 ? 0 : args.length === 1 ? length - start : Math.min(Math.max(toIntegerOrInfinity(args[1]), 0), length - start);
    const removed = Array.from({ length: remove }, (_, offset) => getArrayIndex(node, start + offset));
    const itemStart = Math.min(2, args.length);
    const itemCount = Math.max(args.length - 2, 0);
    replacePieceRange(node, start, remove, insertPlacementPiece(node, args, itemStart));
    setArrayLength(node, length - remove + itemCount);
    return removed;
  },
  reverse() {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.reverse, this, []);
    const overlay = arrayOverlay(node);
    if (arrayLength(overlay) < 2) return node.proxy;
    const pieces = [...piecesOf(overlay)].reverse();
    for (const piece of pieces) {
      piece.start += piece.step * (piece.length - 1);
      piece.step = piece.step === 1 ? -1 : 1;
    }
    replaceAllPieces(overlay, pieces);
    overlay.structural = true;
    overlay.generation += 1;
    overlay.plan = void 0;
    markDirty(node);
    return node.proxy;
  },
  sort(comparator) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.sort, this, [comparator]);
    if (comparator !== void 0 && typeof comparator !== "function")
      throw new TypeError("Comparator must be a function");
    const overlay = arrayOverlay(node);
    const insertedSources = [];
    const insertedIndices = [];
    const insertedValues = [];
    const baseValues = new Array(nodeBase(node).length);
    const order = [];
    for (const piece of piecesOf(overlay)) {
      for (let offset = 0; offset < piece.length; offset++) {
        const sourceIndex = piece.start + piece.step * offset;
        if (piece.kind === "base") {
          order.push(sourceIndex);
          baseValues[sourceIndex] = publicSortValue(node, sourceIndex, insertedSources, insertedIndices);
        } else {
          insertedSources.push(piece.source);
          insertedIndices.push(sourceIndex);
          const token = -insertedSources.length;
          order.push(token);
          insertedValues.push(publicSortValue(node, token, insertedSources, insertedIndices));
        }
      }
    }
    const baseSnapshot = new Map();
    for (const [index, valueIndex] of overlay.baseOverrides ?? [])
      baseSnapshot.set(index, storedValue(node.context, valueIndex));
    const insertSnapshots = new Map();
    for (const [source, overrides] of overlay.insertOverrides ?? []) {
      const snapshot = new Map();
      for (const [index, valueIndex] of overrides) snapshot.set(index, storedValue(node.context, valueIndex));
      insertSnapshots.set(source, snapshot);
    }
    const generation = overlay.generation;
    order.sort((left, right) => {
      const leftValue = left < 0 ? insertedValues[-left - 1] : baseValues[left];
      const rightValue = right < 0 ? insertedValues[-right - 1] : baseValues[right];
      if (typeof comparator === "function") return Number(comparator(leftValue, rightValue));
      const a = String(leftValue);
      const b = String(rightValue);
      return a < b ? -1 : a > b ? 1 : 0;
    });
    if (baseSnapshot.size > 0 || insertSnapshots.size > 0 || (overlay.baseOverrides?.size ?? 0) > 0 || (overlay.insertOverrides?.size ?? 0) > 0) {
      for (const token of order) {
        restoreSortOverride(node, token, insertedSources, insertedIndices, baseSnapshot, insertSnapshots);
      }
    }
    const comparatorWasStructural = overlay.generation !== generation;
    const currentLength = arrayLength(overlay);
    const samePrefix = currentLength >= order.length && order.every((token, index) => sameSortTokenAt(overlay, index, token, insertedSources, insertedIndices));
    if (!samePrefix) {
      replacePieceRange(
        node,
        0,
        Math.min(order.length, currentLength),
        piecesFromSortOrder(order, insertedSources, insertedIndices)
      );
    }
    if (comparatorWasStructural) deduplicateArrayEntries(node);
    return node.proxy;
  },
  fill(supplied, startArg, endArg) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.fill, this, [supplied, startArg, endArg]);
    const length = arrayLength(arrayOverlay(node));
    const start = startArg === void 0 ? 0 : clampIndex(toIntegerOrInfinity(startArg), length);
    const end = endArg === void 0 ? length : clampIndex(toIntegerOrInfinity(endArg), length);
    if (end <= start) return node.proxy;
    const items = Array.from({ length: end - start }, () => clonePlacement(supplied));
    replacePieceRange(node, start, end - start, insertPiece(node, items));
    return node.proxy;
  },
  copyWithin(targetArg, startArg, endArg) {
    const node = mutatorNode(this);
    if (node === void 0) return Reflect.apply(Array.prototype.copyWithin, this, [targetArg, startArg, endArg]);
    const length = arrayLength(arrayOverlay(node));
    const target = clampIndex(toIntegerOrInfinity(targetArg), length);
    const start = clampIndex(toIntegerOrInfinity(startArg), length);
    const end = endArg === void 0 ? length : clampIndex(toIntegerOrInfinity(endArg), length);
    const count = Math.min(Math.max(end - start, 0), length - target);
    const values = Array.from({ length: count }, (_, offset) => clonePlacement(getArrayIndex(node, start + offset)));
    replacePieceRange(node, target, count, insertPiece(node, values));
    return node.proxy;
  }
};
function sortTokenSource(token, sources) {
  return token < 0 ? sources[-token - 1] : void 0;
}
function sortTokenIndex(token, indices) {
  return token < 0 ? indices[-token - 1] : token;
}
function publicSortValue(node, token, sources, indices) {
  const overlay = arrayOverlay(node);
  const source = sortTokenSource(token, sources);
  const sourceIndex = sortTokenIndex(token, indices);
  const overrideIndex = source === void 0 ? overlay.baseOverrides?.get(sourceIndex) : overlay.insertOverrides?.get(source)?.get(sourceIndex);
  const value = overrideIndex !== void 0 ? storedValue(node.context, overrideIndex) : source === void 0 ? nodeBase(node)[sourceIndex] : storedValue(node.context, source.refs[sourceIndex]);
  if (!isContainer(value)) return value;
  return createNode(
    node.context,
    value,
    node,
    source === void 0 ? 1 : 2,
    sourceIndex,
    source,
    source !== void 0 || (overlay.baseOverrides?.has(sourceIndex) ?? false)
  ).proxy;
}
function restoreSortOverride(node, token, sources, indices, baseSnapshot, insertSnapshots) {
  const overlay = arrayOverlay(node);
  const source = sortTokenSource(token, sources);
  const sourceIndex = sortTokenIndex(token, indices);
  if (source === void 0) {
    const existingIndex2 = overlay.baseOverrides?.get(sourceIndex);
    const snapshotValue2 = baseSnapshot.get(sourceIndex);
    if (snapshotValue2 !== void 0) {
      if (existingIndex2 === void 0)
        baseOverridesForWrite(overlay).set(sourceIndex, storeValue(node.context, snapshotValue2));
      else overlay.baseOverrides.set(sourceIndex, replaceStoredValue(node.context, existingIndex2, snapshotValue2));
    } else {
      if (existingIndex2 !== void 0) releaseStoredValue(node.context, existingIndex2);
      overlay.baseOverrides?.delete(sourceIndex);
    }
    return;
  }
  let overrides = overlay.insertOverrides?.get(source);
  const existingIndex = overrides?.get(sourceIndex);
  const snapshotValue = insertSnapshots.get(source)?.get(sourceIndex);
  if (snapshotValue !== void 0) {
    if (overrides === void 0) {
      overrides = new Map();
      insertOverridesForWrite(overlay).set(source, overrides);
    }
    if (existingIndex === void 0) overrides.set(sourceIndex, storeValue(node.context, snapshotValue));
    else overrides.set(sourceIndex, replaceStoredValue(node.context, existingIndex, snapshotValue));
  } else {
    if (existingIndex !== void 0) releaseStoredValue(node.context, existingIndex);
    overrides?.delete(sourceIndex);
    if (overrides?.size === 0) overlay.insertOverrides?.delete(source);
  }
}
function sameSortTokenAt(overlay, logicalIndex, token, sources, indices) {
  const piece = locatePiece(overlay, logicalIndex);
  const sourceIndex = piece.start + piece.step * overlay.locatedOffset;
  const tokenSource = sortTokenSource(token, sources);
  return sourceIndex === sortTokenIndex(token, indices) && (piece.kind === "base" && tokenSource === void 0 || piece.kind === "insert" && piece.source === tokenSource);
}
function deduplicateArrayEntries(node) {
  const overlay = arrayOverlay(node);
  const seenBase = new Set();
  const seenInsert = new Map();
  const next = [];
  let duplicated = false;
  for (const piece of piecesOf(overlay)) {
    for (let offset = 0; offset < piece.length; offset++) {
      const sourceIndex = piece.start + piece.step * offset;
      let seen;
      if (piece.kind === "base") {
        seen = seenBase.has(sourceIndex);
        seenBase.add(sourceIndex);
      } else {
        let indices = seenInsert.get(piece.source);
        if (indices === void 0) {
          indices = new Set();
          seenInsert.set(piece.source, indices);
        }
        seen = indices.has(sourceIndex);
        indices.add(sourceIndex);
      }
      if (seen) {
        duplicated = true;
        appendMergedPiece(
          next,
          insertPiece(node, [clonePlacementStored(entryValueAt(node, piece, sourceIndex), node.context)])[0]
        );
      } else appendMergedPiece(next, singletonPiece(piece, sourceIndex));
    }
  }
  if (!duplicated) return;
  replaceAllPieces(overlay, next);
  overlay.structural = true;
  overlay.generation += 1;
  overlay.plan = void 0;
  markDirty(node);
}
function piecesFromSortOrder(order, sources, indices) {
  const pieces = [];
  for (const token of order) {
    const source = token < 0 ? sources[-token - 1] : void 0;
    const sourceIndex = token < 0 ? indices[-token - 1] : token;
    const previous = pieces.at(-1);
    const sameSource = previous !== void 0 && (source === void 0 && previous.kind === "base" || source !== void 0 && previous.kind === "insert" && previous.source === source);
    if (previous !== void 0 && sameSource) {
      if (previous.length === 1) {
        const step = sourceIndex - previous.start;
        if (step === 1 || step === -1) {
          previous.step = step;
          previous.length = 2;
          continue;
        }
      } else if (previous.start + previous.step * previous.length === sourceIndex) {
        previous.length += 1;
        continue;
      }
    }
    pieces.push(
      source === void 0 ? { kind: "base", start: sourceIndex, length: 1, step: 1 } : { kind: "insert", source, start: sourceIndex, length: 1, step: 1 }
    );
  }
  return pieces;
}
function appendMergedPiece(pieces, piece) {
  const previous = pieces.at(-1);
  const sameSource = previous !== void 0 && previous.kind === piece.kind && (previous.kind === "base" || previous.source === piece.source);
  if (previous !== void 0 && sameSource && previous.length === 1 && piece.length === 1) {
    const step = piece.start - previous.start;
    if (step === 1 || step === -1) {
      previous.step = step;
      previous.length = 2;
      return;
    }
  }
  if (previous !== void 0 && sameSource && previous.step === piece.step && previous.start + previous.step * previous.length === piece.start) {
    previous.length += piece.length;
  } else pieces.push(piece);
}
function singletonPiece(piece, sourceIndex) {
  return piece.kind === "base" ? { kind: "base", start: sourceIndex, length: 1, step: 1 } : { kind: "insert", source: piece.source, start: sourceIndex, length: 1, step: 1 };
}
function clonePlacement(value) {
  const proxyNode = isContainer(value) ? Reflect.get(value, NODE) : void 0;
  return proxyNode === void 0 ? copyJson(value) : clonePlacementNode(proxyNode);
}
function cloneNode(node) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    const overlay = arrayOverlay(node);
    const result2 = [];
    for (const piece of piecesOf(overlay)) {
      for (let offset = 0; offset < piece.length; offset++) {
        const sourceIndex = piece.start + piece.step * offset;
        result2.push(cloneStored(entryValueAt(node, piece, sourceIndex), node.context));
      }
    }
    return result2;
  }
  const result = Object.create(Object.getPrototypeOf(nodeBase(node)));
  for (const key of ownKeys(node))
    defineData(result, key, cloneStored(objectValue(node, key), node.context));
  return result;
}
function cloneStored(value, context) {
  if (!isContainer(value)) return value;
  const node = context.rawNodes.get(value);
  return node === void 0 || !node.dirty && !node.subtreeDirty ? value : cloneNode(node);
}
function clonePlacementNode(node) {
  assertReadable(node.context);
  if (Array.isArray(nodeBase(node))) {
    const overlay = arrayOverlay(node);
    const result2 = [];
    for (const piece of piecesOf(overlay)) {
      for (let offset = 0; offset < piece.length; offset++) {
        const sourceIndex = piece.start + piece.step * offset;
        result2.push(clonePlacementStored(entryValueAt(node, piece, sourceIndex), node.context));
      }
    }
    return result2;
  }
  const result = Object.create(Object.getPrototypeOf(nodeBase(node)));
  for (const key of ownKeys(node))
    defineData(result, key, clonePlacementStored(objectValue(node, key), node.context));
  return result;
}
function clonePlacementStored(value, context) {
  if (!isContainer(value)) return copyJson(value);
  const node = context.rawNodes.get(value);
  return node === void 0 ? copyJson(value) : clonePlacementNode(node);
}
function defineData(target, key, value) {
  DATA_DESCRIPTOR.value = value;
  Object.defineProperty(target, key, DATA_DESCRIPTOR);
  DATA_DESCRIPTOR.value = void 0;
}
function emitOperations(context) {
  const simple = emitSimpleObjectOperations(context);
  if (simple !== void 0) return simple;
  const operations = [];
  const forcedFolds = new Map();
  const emissionPaths = new Map();
  const denseIndices = new Map();
  for (const node of context.dirty) {
    recordDenseArrayPosition(node, denseIndices);
    if (Array.isArray(nodeBase(node))) {
      const overlay = arrayOverlay(node);
      if (!overlay.structural && (overlay.baseOverrides?.size ?? 0) > 0) {
        let candidates = denseIndices.get(node);
        if (candidates === void 0) {
          candidates = { indices: [], bits: void 0, length: nodeBase(node).length };
          denseIndices.set(node, candidates);
        }
        for (const index of overlay.baseOverrides.keys()) addDenseCandidate(candidates, index);
      }
    }
  }
  const denseRegions = new Map();
  for (const [array, candidates] of denseIndices) {
    const path = resolvePath(array);
    if (path === void 0 || path.some((segment) => typeof segment === "string" && RESERVED_SEGMENTS.has(segment))) {
      continue;
    }
    const regions = buildDenseRegions(candidates);
    if (regions.length > 0) denseRegions.set(array, regions);
  }
  for (const node of context.dirty) {
    const path = resolvePath(node);
    if (path === void 0) continue;
    if (hasCoveringDenseRegion(node, path, denseRegions)) continue;
    emissionPaths.set(node, path);
    if (!Array.isArray(nodeBase(node)) && hasReservedMutation(node)) forcedFolds.set(node, path);
    const reservedAt = path.findIndex((segment) => typeof segment === "string" && RESERVED_SEGMENTS.has(segment));
    if (reservedAt >= 0) {
      let ancestor = node;
      for (let depth = path.length; depth > reservedAt; depth--) ancestor = nodeParent(ancestor);
      forcedFolds.set(ancestor, path.slice(0, reservedAt));
    }
  }
  for (const [node, path] of forcedFolds) emissionPaths.set(node, path);
  for (const node of denseRegions.keys()) {
    const path = resolvePath(node);
    if (path !== void 0 && !hasCoveringDenseRegion(node, path, denseRegions)) emissionPaths.set(node, path);
  }
  let maxDepth = 0;
  for (const path of emissionPaths.values()) maxDepth = Math.max(maxDepth, path.length);
  const buckets = Array.from({ length: maxDepth + 1 }, () => []);
  for (const [node, path] of emissionPaths) buckets[path.length].push(node);
  const folded = new Set();
  for (const bucket of buckets) {
    for (const node of bucket) {
      const path = emissionPaths.get(node);
      if (hasPlacementAncestor(node) || hasFoldedAncestor(node, folded)) continue;
      if (forcedFolds.has(node)) {
        emitSet(operations, path, cloneNode(node));
        folded.add(node);
        continue;
      }
      if (Array.isArray(nodeBase(node))) emitArrayOperations(node, path, operations, denseRegions.get(node));
      else emitObjectOperations(node, path, operations);
      if (operations.length > MAX_DELTA_OPERATIONS) {
        locatedDenseArray = void 0;
        return [["r", cloneNode(context.root)]];
      }
    }
  }
  locatedDenseArray = void 0;
  return operations;
}
function emitSimpleObjectOperations(context) {
  const nodes = [];
  for (const node of context.dirty) {
    if (Array.isArray(nodeBase(node)) || hasReservedMutation(node) || hasPlacementAncestor(node)) return void 0;
    for (let parent = nodeParent(node); parent !== void 0; parent = nodeParent(parent)) {
      if (Array.isArray(nodeBase(parent))) return void 0;
    }
    const path = resolvePath(node);
    if (path === void 0) continue;
    for (const segment of path) {
      if (typeof segment === "string" && RESERVED_SEGMENTS.has(segment)) return void 0;
    }
    nodes.push(node);
    if (nodes.length > MAX_SIMPLE_OBJECT_NODES) return void 0;
  }
  for (let index = 1; index < nodes.length; index++) {
    const node = nodes[index];
    const depth = node.preparedPath.length;
    let at = index;
    while (at > 0 && nodes[at - 1].preparedPath.length > depth) {
      nodes[at] = nodes[at - 1];
      at -= 1;
    }
    nodes[at] = node;
  }
  const operations = [];
  let canMaterializeDirectly = true;
  for (const node of nodes) {
    if (emitObjectOperations(node, node.preparedPath, operations)) canMaterializeDirectly = false;
    if (operations.length > MAX_DELTA_OPERATIONS) return [["r", cloneNode(context.root)]];
  }
  context.simpleObjectMaterialization = canMaterializeDirectly;
  return operations;
}
let locatedDenseArray;
let locatedDenseIndex = 0;
function locateDenseArrayPosition(node) {
  let child = node;
  for (let parent = nodeParent(child); parent !== void 0; parent = nodeParent(child)) {
    if (Array.isArray(nodeBase(parent))) {
      const overlay = arrayOverlay(parent);
      if (child.parentKind !== 1 || overlay.structural) return false;
      const sourceIndex = child.parentKey;
      const valueIndex = overlay.baseOverrides?.get(sourceIndex);
      const current = valueIndex === void 0 ? nodeBase(parent)[sourceIndex] : storedValue(parent.context, valueIndex);
      if (current !== nodeBase(child)) return false;
      locatedDenseArray = parent;
      locatedDenseIndex = sourceIndex;
      return true;
    }
    child = parent;
  }
  return false;
}
function hasCoveringDenseRegion(node, path, denseRegions) {
  for (let parent = nodeParent(node); parent !== void 0; parent = nodeParent(parent)) {
    const regions = denseRegions.get(parent);
    if (regions === void 0) continue;
    const parentPath = resolvePath(parent);
    if (parentPath === void 0 || path.length <= parentPath.length) continue;
    let matches = true;
    for (let index2 = 0; index2 < parentPath.length; index2++) {
      if (path[index2] !== parentPath[index2]) {
        matches = false;
        break;
      }
    }
    if (!matches) continue;
    const index = path[parentPath.length];
    if (typeof index === "number" && regionContaining(regions, index)) return true;
  }
  return false;
}
function addDenseCandidate(candidates, index) {
  if (candidates.bits !== void 0) {
    candidates.bits[index] = 1;
    return;
  }
  candidates.indices.push(index);
  if (candidates.indices.length < 256) return;
  candidates.bits = new Uint8Array(candidates.length);
  for (const existing of candidates.indices) candidates.bits[existing] = 1;
  candidates.indices.length = 0;
}
function recordDenseArrayPosition(node, groups) {
  if (!locateDenseArrayPosition(node)) return;
  let candidates = groups.get(locatedDenseArray);
  if (candidates === void 0) {
    candidates = { indices: [], bits: void 0, length: nodeBase(locatedDenseArray).length };
    groups.set(locatedDenseArray, candidates);
  }
  addDenseCandidate(candidates, locatedDenseIndex);
}
function buildDenseRegions(candidates) {
  if (candidates.bits === void 0) return [];
  const regions = [];
  const bits = candidates.bits;
  for (let at = 0; at < bits.length; ) {
    while (at < bits.length && bits[at] === 0) at += 1;
    if (at === bits.length) break;
    const start = at;
    let end = at;
    let count = 0;
    let gap = 0;
    while (at < bits.length) {
      if (bits[at] !== 0) {
        count += 1;
        end = at;
        gap = 0;
      } else if (++gap > 1) break;
      at += 1;
    }
    const length = end - start + 1;
    if (count >= 256 && count * 2 >= length) regions.push({ start, length });
  }
  return regions;
}
function regionContaining(regions, index) {
  for (const region of regions) if (index >= region.start && index < region.start + region.length) return true;
  return false;
}
function hasReservedMutation(node) {
  if (node.writeKey !== void 0 && RESERVED_SEGMENTS.has(node.writeKey)) return true;
  if (node.deleteKey !== void 0 && RESERVED_SEGMENTS.has(node.deleteKey)) return true;
  if (node.writes !== void 0) {
    for (const key of node.writes.keys()) if (RESERVED_SEGMENTS.has(key)) return true;
  }
  if (node.deletes !== void 0) {
    for (const key of node.deletes) if (RESERVED_SEGMENTS.has(key)) return true;
  }
  return false;
}
function hasFoldedAncestor(node, folded) {
  for (let parent = nodeParent(node); parent !== void 0; parent = nodeParent(parent))
    if (folded.has(parent)) return true;
  return false;
}
function hasPlacementAncestor(node) {
  for (let current = node; current !== void 0; current = nodeParent(current)) {
    if (nodeParent(current) !== void 0 && current.parentPlacement) return true;
  }
  return false;
}
function emitObjectWrite(node, path, operations, key, value) {
  const nextPath = [...path, key];
  const before = node.readded?.has(key) || !Object.hasOwn(nodeBase(node), key) ? void 0 : nodeBase(node)[key];
  const after = cloneStored(value, node.context);
  const emitted = emitChangedValue(operations, nextPath, before, after);
  return isContainer(before) && isContainer(after) && !emitted;
}
function emitObjectOperations(node, path, operations) {
  if (node.readded !== void 0) {
    for (const key of node.readded) {
      if (Object.hasOwn(nodeBase(node), key)) operations.push(["d", [...path, key]]);
    }
  }
  let normalizedContainerWrite = false;
  if (node.writeKey !== void 0)
    normalizedContainerWrite = emitObjectWrite(
      node,
      path,
      operations,
      node.writeKey,
      objectValue(node, node.writeKey)
    );
  if (node.writes !== void 0) {
    for (const [key, valueIndex] of node.writes) {
      if (operations.length > MAX_DELTA_OPERATIONS) return normalizedContainerWrite;
      if (emitObjectWrite(node, path, operations, key, storedValue(node.context, valueIndex)))
        normalizedContainerWrite = true;
    }
  }
  if (node.deleteKey !== void 0 && Object.hasOwn(nodeBase(node), node.deleteKey)) {
    operations.push(["d", [...path, node.deleteKey]]);
  }
  if (node.deletes !== void 0) {
    for (const key of node.deletes) {
      if (operations.length > MAX_DELTA_OPERATIONS) return normalizedContainerWrite;
      if (Object.hasOwn(nodeBase(node), key)) operations.push(["d", [...path, key]]);
    }
  }
  return normalizedContainerWrite;
}
function buildArrayPlan(node) {
  const overlay = arrayOverlay(node);
  if (overlay.plan !== void 0) return overlay.plan;
  const base = nodeBase(node);
  const pieces = piecesOf(overlay);
  overlay.plan = orderedArrayPlan(base.length, pieces) ?? elementArrayPlan(base.length, pieces);
  return overlay.plan;
}
function orderedArrayPlan(baseLength, pieces) {
  const ranges = [];
  for (const piece of pieces) {
    if (piece.kind !== "base" || piece.length === 0) continue;
    if (piece.step !== 1 && piece.length > 1) return void 0;
    if (ranges.length > 0 && piece.start <= ranges[ranges.length - 1]) return void 0;
    ranges.push(piece.start, piece.start + piece.length - 1);
  }
  const removeRuns = [];
  let end = baseLength;
  for (let index = ranges.length - 2; index >= -2; index -= 2) {
    const start = index >= 0 ? ranges[index + 1] + 1 : 0;
    if (end > start) removeRuns.push(start, end - start);
    if (index >= 0) end = ranges[index];
  }
  return { removeRuns, permutation: void 0, insertRuns: insertRunsOf(pieces) };
}
function elementArrayPlan(baseLength, pieces) {
  const retained = new Uint8Array(baseLength);
  const targetBase = [];
  for (const piece of pieces) {
    if (piece.kind !== "base") continue;
    for (let offset = 0; offset < piece.length; offset++) {
      const index = piece.start + piece.step * offset;
      retained[index] = 1;
      targetBase.push(index);
    }
  }
  const removeRuns = [];
  for (let end = baseLength; end > 0; ) {
    if (retained[end - 1] !== 0) {
      end -= 1;
      continue;
    }
    let start = end - 1;
    while (start > 0 && retained[start - 1] === 0) start -= 1;
    removeRuns.push(start, end - start);
    end = start;
  }
  const retainedBase = [];
  for (let index = 0; index < retained.length; index++) if (retained[index] !== 0) retainedBase.push(index);
  let permutation;
  if (targetBase.some((value, index) => value !== retainedBase[index])) {
    const positions = new Map(retainedBase.map((value, index) => [value, index]));
    permutation = targetBase.map((value) => positions.get(value));
  }
  return { removeRuns, permutation, insertRuns: insertRunsOf(pieces) };
}
function insertRunsOf(pieces) {
  const insertRuns = [];
  let logicalIndex = 0;
  for (let pieceIndex = 0; pieceIndex < pieces.length; ) {
    const piece = pieces[pieceIndex];
    if (piece.kind === "base") {
      logicalIndex += piece.length;
      pieceIndex += 1;
      continue;
    }
    const startPiece = pieceIndex;
    while (pieceIndex < pieces.length && pieces[pieceIndex].kind === "insert") {
      logicalIndex += pieces[pieceIndex].length;
      pieceIndex += 1;
    }
    insertRuns.push(logicalIndex - rangeLength(pieces, startPiece, pieceIndex), startPiece, pieceIndex);
  }
  return insertRuns;
}
function rangeLength(pieces, start, end) {
  let length = 0;
  for (let index = start; index < end; index++) length += pieces[index].length;
  return length;
}
function emitArrayOperations(node, path, operations, denseRegions) {
  const overlay = arrayOverlay(node);
  const base = nodeBase(node);
  if (denseRegions !== void 0) {
    for (const region of denseRegions) {
      if (operations.length > MAX_DELTA_OPERATIONS) return;
      operations.push(["p", path, region.start, region.length, cloneArrayRegion(node, region)]);
    }
  }
  if (overlay.structural) {
    const plan = buildArrayPlan(node);
    for (let index = 0; index < plan.removeRuns.length; index += 2) {
      if (operations.length > MAX_DELTA_OPERATIONS) return;
      operations.push(["p", path, plan.removeRuns[index], plan.removeRuns[index + 1], []]);
    }
    if (plan.permutation !== void 0) operations.push(["m", path, plan.permutation]);
    const pieces = piecesOf(overlay);
    for (let run = 0; run < plan.insertRuns.length; run += 3) {
      if (operations.length > MAX_DELTA_OPERATIONS) return;
      const logicalIndex = plan.insertRuns[run];
      const items = [];
      for (let pieceIndex = plan.insertRuns[run + 1]; pieceIndex < plan.insertRuns[run + 2]; pieceIndex++) {
        const piece = pieces[pieceIndex];
        for (let offset = 0; offset < piece.length; offset++) {
          const sourceIndex = piece.start + piece.step * offset;
          items.push(cloneStored(entryValueAt(node, piece, sourceIndex), node.context));
        }
      }
      operations.push(["p", path, logicalIndex, 0, items]);
    }
  }
  for (const [baseIndex, valueIndex] of overlay.baseOverrides ?? []) {
    if (operations.length > MAX_DELTA_OPERATIONS) return;
    const index = findEntryIndex(overlay, 1, baseIndex, void 0);
    if (index === void 0 || denseRegions !== void 0 && regionContaining(denseRegions, index)) continue;
    emitChangedValue(
      operations,
      [...path, index],
      base[baseIndex],
      cloneStored(storedValue(node.context, valueIndex), node.context)
    );
  }
}
function cloneArrayRegion(node, region) {
  const overlay = arrayOverlay(node);
  const result = [];
  for (let index = region.start; index < region.start + region.length; index++) {
    const piece = locatePiece(overlay, index);
    const sourceIndex = piece.start + piece.step * overlay.locatedOffset;
    result.push(cloneStored(entryValueAt(node, piece, sourceIndex), node.context));
  }
  return result;
}
function emitChangedValue(operations, path, before, after) {
  if (!isContainer(after) && before === after) return false;
  if (isContainer(before) && isContainer(after) && equalTrustedJson(before, after)) return false;
  if (typeof before === "string" && typeof after === "string") {
    if (after.length > before.length && after.slice(0, before.length) === before) {
      operations.push(["a", path, after.slice(before.length)]);
      return true;
    }
    const shared = overlap(before, after, 65536);
    if (shared > 0) {
      operations.push(["t", path, before.length - shared]);
      if (after.length > shared) operations.push(["a", path, after.slice(shared)]);
      return true;
    }
  }
  operations.push(["s", path, after]);
  return true;
}
function emitSet(operations, path, value) {
  if (path.length === 0) operations.push(["r", value]);
  else operations.push(["s", path, value]);
}
function resolvePath(node) {
  if (node.preparedPath !== void 0) return node.preparedPath;
  const parent = nodeParent(node);
  if (parent === void 0) {
    node.preparedPath = [];
    return node.preparedPath;
  }
  const parentPath = resolvePath(parent);
  if (parentPath === void 0) return void 0;
  if (node.parentKind === 0) {
    const key = node.parentKey;
    if (!objectHas(parent, key) || objectValue(parent, key) !== nodeBase(node)) return void 0;
    node.preparedPath = [...parentPath, key];
    return node.preparedPath;
  }
  const overlay = arrayOverlay(parent);
  const index = findEntryIndex(overlay, node.parentKind, node.parentKey, node.parentSource);
  if (index === void 0) return void 0;
  const piece = locatePiece(overlay, index);
  if (entryValueAt(parent, piece, node.parentKey) !== nodeBase(node)) return void 0;
  node.preparedPath = [...parentPath, index];
  return node.preparedPath;
}
function ensurePieceLocations(overlay) {
  if (overlay.baseLocations !== void 0) return;
  overlay.baseLocations = [];
  overlay.insertLocations = new Map();
  let logicalStart = 0;
  for (const piece of piecesOf(overlay)) {
    const last = piece.start + piece.step * (piece.length - 1);
    const location = {
      piece,
      logicalStart,
      minimum: Math.min(piece.start, last),
      maximum: Math.max(piece.start, last)
    };
    if (piece.kind === "base") overlay.baseLocations.push(location);
    else {
      let locations = overlay.insertLocations.get(piece.source);
      if (locations === void 0) {
        locations = [];
        overlay.insertLocations.set(piece.source, locations);
      }
      locations.push(location);
    }
    logicalStart += piece.length;
  }
  const byMinimum = (left, right) => left.minimum - right.minimum;
  overlay.baseLocations.sort(byMinimum);
  for (const locations of overlay.insertLocations.values()) locations.sort(byMinimum);
}
function findEntryIndex(overlay, kind, sourceIndex, source) {
  if (kind === 1 && !overlay.structural) return sourceIndex;
  ensurePieceLocations(overlay);
  const locations = kind === 1 ? overlay.baseLocations : overlay.insertLocations.get(source);
  if (locations === void 0) return void 0;
  let low = 0;
  let high = locations.length;
  while (low < high) {
    const middle = low + high >>> 1;
    if (locations[middle].minimum <= sourceIndex) low = middle + 1;
    else high = middle;
  }
  const location = locations[low - 1];
  if (location === void 0 || sourceIndex > location.maximum) return void 0;
  const offset = (sourceIndex - location.piece.start) / location.piece.step;
  return offset >= 0 && offset < location.piece.length ? location.logicalStart + offset : void 0;
}
function ensureOperations(context) {
  if (context.ops !== void 0) return context.ops;
  assertReadable(context);
  if (context.replacement) {
    const base = context.tracker.deref()?.replacementBase(context);
    context.replacementNoop = base !== void 0 && equalTrustedJson(base, nodeBase(context.root));
    context.ops = context.replacementNoop ? [] : [["r", nodeBase(context.root)]];
  } else context.ops = emitOperations(context);
  return context.ops;
}
function equalTrustedJson(left, right) {
  if (left === right) return true;
  if (!isContainer(left) || !isContainer(right) || Array.isArray(left) !== Array.isArray(right)) return false;
  if (Array.isArray(left)) {
    const other2 = right;
    if (left.length !== other2.length) return false;
    for (let index = 0; index < left.length; index++)
      if (!equalTrustedJson(left[index], other2[index])) return false;
    return true;
  }
  const other = right;
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(other).length) return false;
  for (const key of keys) if (!Object.hasOwn(other, key) || !equalTrustedJson(left[key], other[key])) return false;
  return true;
}
function abortContext(context) {
  if (context.status.value === "aborted" || context.status.value === "consumed" || context.status.value === "stale")
    return;
  context.status.value = "aborted";
  clearContext(context);
}
function releaseOverlayReferences(context) {
  context.tracker.deref()?.releaseContext(context);
  if (locatedDenseArray?.context === context) locatedDenseArray = void 0;
  context.overlayReleased = true;
  context.dirty.length = 0;
  context.nodes.length = 0;
  context.rawNodes = void 0;
  context.ops = void 0;
  context.root = void 0;
  context.bases = void 0;
  context.stored = void 0;
  context.baseValue = void 0;
}
function clearContext(context) {
  context.tracker.deref()?.releaseContext(context);
  if (locatedDenseArray?.context === context) locatedDenseArray = void 0;
  for (const node of context.nodes) {
    if (Array.isArray(node.target)) node.target.length = 0;
    Reflect.deleteProperty(node.target, NODE);
    node.writes?.clear();
    node.deletes?.clear();
    node.readded?.clear();
    if (node.array?.pieces !== void 0) node.array.pieces.length = 0;
    if (node.array !== void 0) {
      node.array.root = void 0;
      node.array.baseLocations = void 0;
      node.array.insertLocations = void 0;
    }
    node.array?.baseOverrides?.clear();
    node.array?.insertOverrides?.clear();
    node.parentSource = void 0;
    node.target = RELEASED;
    node.proxy = RELEASED;
    node.writeKey = void 0;
    node.writeValue = void 0;
    node.writes = void 0;
    node.deleteKey = void 0;
    node.deletes = void 0;
    node.readded = void 0;
    node.array = void 0;
    node.subtreeDirty = void 0;
    node.preparedPath = void 0;
  }
  context.dirty.length = 0;
  context.nodes.length = 0;
  context.rawNodes = void 0;
  context.ops = void 0;
  context.root = void 0;
  context.bases = void 0;
  context.stored = void 0;
  context.baseValue = void 0;
  context.overlayReleased = true;
}
function isContainer(value) {
  return value !== null && typeof value === "object";
}
export {
  track
};
