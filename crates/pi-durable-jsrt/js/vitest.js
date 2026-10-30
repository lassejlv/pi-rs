// vitest — the test API (vitest 4) for running unchanged upstream test files inside pi-jsrt.
//
// Collection happens while the test module is imported (describe bodies run immediately); the
// runner (examples/embedded_stock.rs) then calls `__runner.run()`, which runs the tree
// sequentially like `vitest --run` with one file: hooks in `sequence.hooks: "stack"` order,
// `testTimeout` 5000 ms and `hookTimeout` 10000 ms unless a test or hook passes its own.
// Timeouts and the runner's own yields use real timers (`__jsrtTimers.realTimeout`), so fake
// timers never stall the runner. Fake timers drive pi-jsrt's real timer queue through
// `__jsrtTimers` (RuntimeOptions::timer_control) and fake `Date`, `performance.now` and
// `process.hrtime` like vitest's defaults (every timer API except nextTick/queueMicrotask).
//
// `vi.mock(specifier, factory)` works with the runner's hoisting transform (src/hoist.rs).
// Not supported: automocking (`vi.mock` without a factory), `vi.doMock` (throws),
// snapshots, `expect.soft`, `test.extend` fixtures, concurrent execution (`.concurrent` runs
// sequentially), benchmarks (`bench` registers nothing).

import { inspect } from "node:util";
import nodeModule from "node:module";
import { fileURLToPath } from "node:url";
import { call } from "pi-durable:bridge";

const T = globalThis.__jsrtTimers;
const RealDate = globalThis.Date;
const realDateNow = RealDate.now.bind(RealDate);
const realTimeout = (fn, ms) => (T ? T.realTimeout(fn, ms) : setTimeout(fn, ms));
const realClear = (id) => clearTimeout(id);
const realYield = () => new Promise((resolve) => realTimeout(resolve, 0));
const hasOwn = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
const fmt = (value) => {
  try {
    return inspect(value, { depth: 6, breakLength: Infinity });
  } catch {
    return String(value);
  }
};

/** The two values as JSON (with lone surrogates and types visible) when that shows their difference. */
function jsonDiff(received, expected) {
  try {
    const r = JSON.stringify(received);
    const e = JSON.stringify(expected);
    if (r === undefined || e === undefined) return "";
    return r === e ? `\n(JSON-identical: the difference is in identity, prototypes, undefined members or asymmetric matchers)` : `\n- expected ${e}\n+ received ${r}`;
  } catch {
    return "";
  }
}

const DEFAULT_TEST_TIMEOUT = 5000;
const DEFAULT_HOOK_TIMEOUT = 10000;

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

class Suite {
  constructor(name, parent, mode, each) {
    this.name = name;
    this.parent = parent;
    this.mode = mode; // "run" | "skip" | "only" | "todo"
    this.children = [];
    this.hooks = { beforeAll: [], afterAll: [], beforeEach: [], afterEach: [] };
    this.each = each;
  }
  titles() {
    const out = [];
    for (let s = this; s && s.parent; s = s.parent) out.unshift(s.name);
    return out;
  }
}

const root = new Suite("", null, "run");
let currentSuite = root;
let hasOnly = false;
const pendingCollection = [];
const collectionErrors = [];

function parseOptions(a, b) {
  // (fn, timeout) | (fn, options) | (options, fn)
  let fn;
  let options = {};
  if (typeof a === "function") {
    fn = a;
    if (typeof b === "number") options = { timeout: b };
    else if (b && typeof b === "object") options = b;
  } else if (a && typeof a === "object") {
    options = a;
    if (typeof b === "function") fn = b;
  }
  return { fn, options };
}

function nameOf(name) {
  if (typeof name === "function") return name.name || "<anonymous>";
  return String(name);
}

function registerTest(name, a, b, flags) {
  const { fn, options } = parseOptions(a, b);
  let mode = flags.mode ?? "run";
  if (options.skip) mode = "skip";
  if (options.only) mode = "only";
  if (options.todo || (fn === undefined && mode === "run")) mode = "todo";
  if (mode === "only") hasOnly = true;
  currentSuite.children.push({
    type: "test",
    name: nameOf(name),
    fn,
    mode,
    fails: flags.fails === true || options.fails === true,
    timeout: options.timeout,
    suite: currentSuite,
  });
}

function registerSuite(name, a, b, flags) {
  const { fn, options } = parseOptions(a, b);
  let mode = flags.mode ?? "run";
  if (options.skip) mode = "skip";
  if (options.only) mode = "only";
  if (options.todo) mode = "todo";
  if (mode === "only") hasOnly = true;
  const suite = new Suite(nameOf(name), currentSuite, mode);
  suite.timeout = options.timeout;
  currentSuite.children.push(suite);
  if (typeof fn !== "function") return;
  const previous = currentSuite;
  currentSuite = suite;
  try {
    const result = fn();
    if (result && typeof result.then === "function") {
      pendingCollection.push(
        Promise.resolve(result).catch((error) => {
          collectionErrors.push(error);
        }),
      );
    }
  } catch (error) {
    collectionErrors.push(error);
  } finally {
    currentSuite = previous;
  }
}

function formatTitle(template, args, index) {
  if (typeof template !== "string") return nameOf(template);
  let i = 0;
  let out = template.replace(/%[sdifjoOc#%]/g, (spec) => {
    if (spec === "%%") return "%";
    if (spec === "%#") return String(index);
    if (i >= args.length) return spec;
    const value = args[i++];
    switch (spec) {
      case "%s":
        return typeof value === "string" ? value : fmt(value);
      case "%d":
        return String(Number(value));
      case "%i":
        return String(Number.parseInt(value, 10));
      case "%f":
        return String(Number.parseFloat(value));
      case "%j":
        try {
          return JSON.stringify(value);
        } catch {
          return "[Circular]";
        }
      case "%c":
        return "";
      default:
        return fmt(value);
    }
  });
  if (args.length === 1 && args[0] !== null && typeof args[0] === "object" && !Array.isArray(args[0])) {
    const row = args[0];
    out = out.replace(/\$([A-Za-z_][\w.]*)/g, (match, path) => {
      let v = row;
      for (const part of path.split(".")) {
        if (v == null || !(part in Object(v))) return match;
        v = v[part];
      }
      return typeof v === "string" ? v : fmt(v);
    });
  }
  return out;
}

function eachRows(table) {
  return Array.from(table, (row) => (Array.isArray(row) ? row : [row]));
}

function makeEach(register, flags) {
  return (table, ...values) => {
    let rows;
    if (Array.isArray(table) && Array.isArray(table.raw)) {
      // Tagged template table: first row holds the names.
      const headings = table.join("").trim().split(/\s*\|\s*/).filter(Boolean);
      rows = [];
      for (let i = 0; i < values.length; i += headings.length) {
        const row = {};
        headings.forEach((h, j) => (row[h] = values[i + j]));
        rows.push([row]);
      }
    } else {
      rows = eachRows(table);
    }
    return (name, a, b) => {
      const { fn, options } = parseOptions(a, b);
      rows.forEach((args, index) => {
        const title = formatTitle(name, args, index);
        register(title, fn === undefined ? undefined : () => fn(...args), options, flags);
      });
    };
  };
}

function api(register, flags = {}) {
  const fn = (name, a, b) => register(name, a, b, flags);
  const derive = (extra) => api(register, { ...flags, ...extra });
  Object.defineProperties(fn, {
    skip: { get: () => derive({ mode: "skip" }) },
    only: { get: () => derive({ mode: "only" }) },
    todo: { get: () => derive({ mode: "todo" }) },
    fails: { get: () => derive({ fails: true }) },
    concurrent: { get: () => derive({}) },
    sequential: { get: () => derive({}) },
    shuffle: { get: () => derive({}) },
    each: { get: () => makeEach(register, flags) },
    for: {
      get: () => (table) => (name, a, b) => {
        const { fn: body, options } = parseOptions(a, b);
        Array.from(table).forEach((row, index) => {
          const args = Array.isArray(row) ? row : [row];
          register(formatTitle(name, args, index), body === undefined ? undefined : (context) => body(row, context), options, flags);
        });
      },
    },
    skipIf: { get: () => (condition) => (condition ? derive({ mode: "skip" }) : fn) },
    runIf: { get: () => (condition) => (condition ? fn : derive({ mode: "skip" })) },
    extend: {
      get: () => () => {
        throw new Error("test.extend fixtures are not supported by the embedded vitest shim");
      },
    },
  });
  return fn;
}

export const it = api(registerTest);
export const test = it;
export const describe = api(registerSuite);
export const suite = describe;

export function bench() {}

function addHook(kind, fn, timeout) {
  currentSuite.hooks[kind].push({ fn, timeout });
}
export const beforeAll = (fn, timeout) => addHook("beforeAll", fn, timeout);
export const afterAll = (fn, timeout) => addHook("afterAll", fn, timeout);
export const beforeEach = (fn, timeout) => addHook("beforeEach", fn, timeout);
export const afterEach = (fn, timeout) => addHook("afterEach", fn, timeout);

let currentTest;
export function onTestFinished(fn) {
  if (!currentTest) throw new Error("onTestFinished() can only be called inside a test");
  currentTest.finished.push(fn);
}
export function onTestFailed(fn) {
  if (!currentTest) throw new Error("onTestFailed() can only be called inside a test");
  currentTest.failed.push(fn);
}

// ---------------------------------------------------------------------------
// Equality (jest/vitest semantics)
// ---------------------------------------------------------------------------

const isAsymmetric = (v) => v !== null && typeof v === "object" && typeof v.asymmetricMatch === "function";

function ownKeys(obj, strict) {
  const keys = Object.keys(obj);
  for (const sym of Object.getOwnPropertySymbols(obj)) {
    if (Object.getOwnPropertyDescriptor(obj, sym)?.enumerable) keys.push(sym);
  }
  return strict ? keys : keys.filter((k) => obj[k] !== undefined);
}

function typeEquals(a, b) {
  if (Array.isArray(a) && Array.isArray(b)) return true;
  return Object.getPrototypeOf(a) === Object.getPrototypeOf(b);
}

function isIterable(v) {
  return v != null && typeof v === "object" && typeof v[Symbol.iterator] === "function" && !Array.isArray(v) && !ArrayBuffer.isView(v);
}

export function equals(a, b, strict = false) {
  return eq(a, b, strict, [], []);
}

const ObjectProto = Object.prototype;
const ArrayProto = Array.prototype;

function eq(a, b, strict, aStack, bStack) {
  const aAsym = isAsymmetric(a);
  const bAsym = isAsymmetric(b);
  if (aAsym && !bAsym) return a.asymmetricMatch(b);
  if (bAsym && !aAsym) return b.asymmetricMatch(a);
  if (Object.is(a, b)) return true;
  if (a === null || b === null || typeof a !== "object" || typeof b !== "object") return false;
  // Fast paths for plain arrays and objects (the bulk of JSON-like values).
  const pa = Object.getPrototypeOf(a);
  const pb = Object.getPrototypeOf(b);
  if (pa === ArrayProto && pb === ArrayProto) return eqArray(a, b, strict, aStack, bStack);
  if ((pa === ObjectProto || pa === null) && (pb === ObjectProto || pb === null) && (!strict || pa === pb)) {
    return eqPlain(a, b, strict, aStack, bStack);
  }
  const className = Object.prototype.toString.call(a);
  if (className !== Object.prototype.toString.call(b)) return false;
  switch (className) {
    case "[object Boolean]":
    case "[object String]":
    case "[object Number]":
      return Object.is(a.valueOf(), b.valueOf());
    case "[object Date]": {
      const x = +a;
      const y = +b;
      return x === y || (Number.isNaN(x) && Number.isNaN(y));
    }
    case "[object RegExp]":
      return a.source === b.source && a.flags === b.flags;
  }
  let i = aStack.length;
  while (i--) {
    if (aStack[i] === a) return bStack[i] === b;
    if (bStack[i] === b) return false;
  }
  aStack.push(a);
  bStack.push(b);
  try {
    return eqObjects(a, b, strict, aStack, bStack, className);
  } finally {
    aStack.pop();
    bStack.pop();
  }
}

function onStack(a, b, aStack, bStack) {
  let i = aStack.length;
  while (i--) {
    if (aStack[i] === a) return bStack[i] === b ? 1 : -1;
    if (bStack[i] === b) return -1;
  }
  return 0;
}

function eqArray(a, b, strict, aStack, bStack) {
  if (a.length !== b.length) return false;
  const seen = onStack(a, b, aStack, bStack);
  if (seen !== 0) return seen === 1;
  if (Object.getOwnPropertySymbols(a).length > 0 || Object.getOwnPropertySymbols(b).length > 0 || Object.keys(a).length !== Object.keys(b).length) {
    return eqObjects(a, b, strict, aStack, bStack, "[object Array]");
  }
  aStack.push(a);
  bStack.push(b);
  try {
    for (let i = 0; i < a.length; i++) {
      if (strict && i in a !== i in b) return false;
      if (!eq(a[i], b[i], strict, aStack, bStack)) return false;
    }
    return true;
  } finally {
    aStack.pop();
    bStack.pop();
  }
}

function eqPlain(a, b, strict, aStack, bStack) {
  const seen = onStack(a, b, aStack, bStack);
  if (seen !== 0) return seen === 1;
  if (Object.getOwnPropertySymbols(a).length > 0 || Object.getOwnPropertySymbols(b).length > 0) {
    return eqObjects(a, b, strict, aStack, bStack, "[object Object]");
  }
  aStack.push(a);
  bStack.push(b);
  try {
    let count = 0;
    for (const key of Object.keys(a)) {
      const value = a[key];
      if (!strict && value === undefined) continue;
      count++;
      if (!hasOwn(b, key)) return false;
      if (!eq(value, b[key], strict, aStack, bStack)) return false;
    }
    let countB = 0;
    for (const key of Object.keys(b)) {
      if (strict || b[key] !== undefined) countB++;
    }
    return count === countB;
  } finally {
    aStack.pop();
    bStack.pop();
  }
}

function eqObjects(a, b, strict, aStack, bStack, className) {
  if (strict && !typeEquals(a, b)) return false;
  if (a instanceof Error && b instanceof Error) {
    if (a.name !== b.name || a.message !== b.message) return false;
    if (!eq(a.cause, b.cause, strict, aStack, bStack)) return false;
    if (a instanceof AggregateError && b instanceof AggregateError && !eq(a.errors, b.errors, strict, aStack, bStack)) return false;
  }
  if (className === "[object Array]" && a.length !== b.length) return false;
  if (a instanceof ArrayBuffer && b instanceof ArrayBuffer) {
    return eq(new Uint8Array(a), new Uint8Array(b), strict, aStack, bStack);
  }
  if (a instanceof DataView && b instanceof DataView) {
    return eq(new Uint8Array(a.buffer, a.byteOffset, a.byteLength), new Uint8Array(b.buffer, b.byteOffset, b.byteLength), strict, aStack, bStack);
  }
  if (a instanceof Map && b instanceof Map) {
    if (a.size !== b.size) return false;
    for (const [key, value] of a) {
      if (b.has(key)) {
        if (!eq(value, b.get(key), strict, aStack, bStack)) return false;
        continue;
      }
      let found = false;
      for (const [otherKey, otherValue] of b) {
        if (eq(key, otherKey, strict, aStack, bStack) && eq(value, otherValue, strict, aStack, bStack)) {
          found = true;
          break;
        }
      }
      if (!found) return false;
    }
    return true;
  }
  if (a instanceof Set && b instanceof Set) {
    if (a.size !== b.size) return false;
    for (const value of a) {
      if (b.has(value)) continue;
      let found = false;
      for (const other of b) {
        if (eq(value, other, strict, aStack, bStack)) {
          found = true;
          break;
        }
      }
      if (!found) return false;
    }
    return true;
  }
  if (isIterable(a) && isIterable(b) && !(a instanceof Error)) {
    const left = [...a];
    const right = [...b];
    if (!eq(left, right, strict, aStack, bStack)) return false;
  }
  const aKeys = ownKeys(a, strict);
  const bKeys = ownKeys(b, strict);
  if (aKeys.length !== bKeys.length) return false;
  for (const key of aKeys) {
    if (!hasOwn(b, key) && !(typeof key === "symbol" && Object.getOwnPropertyDescriptor(b, key))) return false;
    if (!eq(a[key], b[key], strict, aStack, bStack)) return false;
  }
  return true;
}

function subsetEquals(object, subset, seen = new WeakMap()) {
  if (isAsymmetric(subset)) return subset.asymmetricMatch(object);
  if (subset === null || typeof subset !== "object") return equals(object, subset);
  if (Array.isArray(subset)) {
    if (!Array.isArray(object) || object.length !== subset.length) return false;
    return subset.every((item, index) => subsetEquals(object[index], item, seen));
  }
  if (subset instanceof Map || subset instanceof Set || subset instanceof Date || subset instanceof RegExp || ArrayBuffer.isView(subset)) {
    return equals(object, subset);
  }
  if (object === null || typeof object !== "object") return false;
  if (seen.has(subset)) return seen.get(subset) === object;
  seen.set(subset, object);
  try {
    for (const key of ownKeys(subset, true)) {
      if (!(key in object)) return false;
      if (!subsetEquals(object[key], subset[key], seen)) return false;
    }
    if (subset instanceof Error && object instanceof Error) return subset.message === object.message && subsetEquals(object, { ...subset }, seen);
    return true;
  } finally {
    seen.delete(subset);
  }
}

// ---------------------------------------------------------------------------
// Asymmetric matchers
// ---------------------------------------------------------------------------

class AsymmetricMatcher {
  constructor(sample, inverse = false) {
    this.sample = sample;
    this.inverse = inverse;
  }
  asymmetricMatch(other) {
    const result = this.match(other);
    return this.inverse ? !result : result;
  }
  [Symbol.for("nodejs.util.inspect.custom")]() {
    return `${this.inverse ? "Not" : ""}${this.constructor.name}(${fmt(this.sample)})`;
  }
}

function typeName(ctor) {
  return ctor?.name;
}

class Any extends AsymmetricMatcher {
  match(other) {
    const s = this.sample;
    if (s === String) return typeof other === "string" || other instanceof String;
    if (s === Number) return typeof other === "number" || other instanceof Number;
    if (s === Boolean) return typeof other === "boolean" || other instanceof Boolean;
    if (s === BigInt) return typeof other === "bigint";
    if (s === Symbol) return typeof other === "symbol";
    if (s === Function) return typeof other === "function";
    if (s === Object) return typeof other === "object" && other !== null;
    return other instanceof s;
  }
  toString() {
    return `Any<${typeName(this.sample)}>`;
  }
}

class Anything extends AsymmetricMatcher {
  match(other) {
    return other !== null && other !== undefined;
  }
}

class ObjectContaining extends AsymmetricMatcher {
  match(other) {
    if (other === null || other === undefined) return false;
    for (const key of ownKeys(this.sample, true)) {
      if (!(key in Object(other)) || !equals(other[key], this.sample[key])) return false;
    }
    return true;
  }
}

class ArrayContaining extends AsymmetricMatcher {
  match(other) {
    if (!Array.isArray(other)) return false;
    return this.sample.every((item) => other.some((value) => equals(value, item)));
  }
}

class StringContaining extends AsymmetricMatcher {
  match(other) {
    return typeof other === "string" && other.includes(this.sample);
  }
}

class StringMatching extends AsymmetricMatcher {
  match(other) {
    return typeof other === "string" && (this.sample instanceof RegExp ? this.sample.test(other) : other.includes(this.sample));
  }
}

class CloseTo extends AsymmetricMatcher {
  constructor(sample, precision = 2, inverse = false) {
    super(sample, inverse);
    this.precision = precision;
  }
  match(other) {
    if (typeof other !== "number") return false;
    if (other === Infinity && this.sample === Infinity) return true;
    if (other === -Infinity && this.sample === -Infinity) return true;
    return Math.abs(this.sample - other) < 10 ** -this.precision / 2;
  }
}

class Satisfies extends AsymmetricMatcher {
  match(other) {
    return this.sample(other) === true;
  }
}

// ---------------------------------------------------------------------------
// expect
// ---------------------------------------------------------------------------

export class AssertionError extends Error {
  constructor(message, actual, expected) {
    super(message);
    this.name = "AssertionError";
    if (arguments.length > 1) this.actual = actual;
    if (arguments.length > 2) this.expected = expected;
  }
}

let assertionCalls = 0;
let expectedAssertions;
let expectHasAssertions = false;

const messageOf = (thrown) => (thrown !== null && typeof thrown === "object" && "message" in thrown ? String(thrown.message) : String(thrown));

function throwMatches(thrown, expected) {
  if (expected === undefined) return true;
  if (typeof expected === "function") return thrown instanceof expected;
  if (typeof expected === "string") return messageOf(thrown).includes(expected);
  if (expected instanceof RegExp) return expected.test(messageOf(thrown));
  if (isAsymmetric(expected)) return expected.asymmetricMatch(thrown);
  if (expected instanceof Error) return messageOf(thrown) === expected.message;
  if (expected !== null && typeof expected === "object") return subsetEquals(thrown, expected);
  return false;
}

function isMock(fn) {
  return typeof fn === "function" && fn._isMockFunction === true;
}

function requireMock(received, matcher) {
  if (!isMock(received)) throw new AssertionError(`${matcher}: ${fmt(received)} is not a spy or a call to a spy`);
  return received.mock;
}

function getPath(object, path) {
  const parts = Array.isArray(path) ? path : String(path).replace(/\[(\w+)\]/g, ".$1").split(".").filter((p) => p !== "");
  let value = object;
  for (const part of parts) {
    if (value === null || value === undefined) return { found: false };
    if (!(part in Object(value))) return { found: false };
    value = value[part];
  }
  return { found: true, value };
}

const matchers = {
  toBe(received, expected) {
    return { pass: Object.is(received, expected), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be ${fmt(expected)} // Object.is equality` };
  },
  toEqual(received, expected) {
    return { pass: equals(received, expected), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to deeply equal ${fmt(expected)}${not ? "" : jsonDiff(received, expected)}` };
  },
  toStrictEqual(received, expected) {
    return { pass: equals(received, expected, true), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to strictly equal ${fmt(expected)}` };
  },
  toMatchObject(received, expected) {
    if (received === null || typeof received !== "object") throw new AssertionError(`toMatchObject: received value must be a non-null object, got ${fmt(received)}`);
    return { pass: subsetEquals(received, expected), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to match object ${fmt(expected)}` };
  },
  toBeUndefined(received) {
    return { pass: received === undefined, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be undefined` };
  },
  toBeDefined(received) {
    return { pass: received !== undefined, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be defined` };
  },
  toBeNull(received) {
    return { pass: received === null, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be null` };
  },
  toBeNaN(received) {
    return { pass: Number.isNaN(received), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be NaN` };
  },
  toBeTruthy(received) {
    return { pass: !!received, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be truthy` };
  },
  toBeFalsy(received) {
    return { pass: !received, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be falsy` };
  },
  toBeTypeOf(received, type) {
    return { pass: typeof received === type, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be type of ${type}` };
  },
  toBeInstanceOf(received, ctor) {
    return { pass: received instanceof ctor, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be an instance of ${ctor?.name}` };
  },
  toBeGreaterThan(received, n) {
    return { pass: received > n, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be greater than ${fmt(n)}` };
  },
  toBeGreaterThanOrEqual(received, n) {
    return { pass: received >= n, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be greater than or equal to ${fmt(n)}` };
  },
  toBeLessThan(received, n) {
    return { pass: received < n, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be less than ${fmt(n)}` };
  },
  toBeLessThanOrEqual(received, n) {
    return { pass: received <= n, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be less than or equal to ${fmt(n)}` };
  },
  toBeCloseTo(received, expected, precision = 2) {
    return { pass: new CloseTo(expected, precision).asymmetricMatch(received), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be close to ${fmt(expected)}` };
  },
  toBeOneOf(received, options) {
    return { pass: Array.from(options).some((o) => equals(received, o)), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to be one of ${fmt(options)}` };
  },
  toSatisfy(received, predicate) {
    return { pass: !!predicate(received), message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to satisfy ${predicate.name || "the predicate"}` };
  },
  toHaveLength(received, length) {
    const actual = received?.length;
    return { pass: actual === length, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to have a length of ${length} but got ${actual}` };
  },
  toContain(received, item) {
    let pass;
    if (typeof received === "string") pass = received.includes(String(item));
    else if (received != null && typeof received[Symbol.iterator] === "function") pass = Array.from(received).some((v) => v === item);
    else throw new AssertionError(`toContain: ${fmt(received)} is not a string or iterable`);
    return { pass, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to contain ${fmt(item)}` };
  },
  toContainEqual(received, item) {
    const pass = Array.from(received).some((v) => equals(v, item));
    return { pass, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to contain an item equal to ${fmt(item)}` };
  },
  toMatch(received, pattern) {
    if (typeof received !== "string") throw new AssertionError(`toMatch: received value must be a string, got ${fmt(received)}`);
    const pass = typeof pattern === "string" ? received.includes(pattern) : pattern.test(received);
    return { pass, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to match ${fmt(pattern)}` };
  },
  toHaveProperty(received, path, ...value) {
    const { found, value: actual } = getPath(received, path);
    const pass = found && (value.length === 0 || equals(actual, value[0]));
    return { pass, message: (not) => `expected ${fmt(received)} ${not ? "not " : ""}to have property ${fmt(path)}${value.length ? ` with value ${fmt(value[0])}` : ""}` };
  },
  toThrow(received, expected) {
    let thrown;
    let threw = false;
    if (this.mode === "rejects") {
      threw = true;
      thrown = received;
    } else {
      if (typeof received !== "function") throw new AssertionError(`toThrow: ${fmt(received)} is not a function`);
      try {
        received();
      } catch (error) {
        threw = true;
        thrown = error;
      }
    }
    const pass = threw && throwMatches(thrown, expected);
    return {
      pass,
      message: (not) =>
        threw
          ? `expected ${fmt(thrown)} ${not ? "not " : ""}to match ${expected === undefined ? "a thrown error" : fmt(expected)}`
          : `expected function to throw${expected === undefined ? "" : ` ${fmt(expected)}`}, but it did not throw`,
    };
  },
  toHaveBeenCalled(received) {
    const mock = requireMock(received, "toHaveBeenCalled");
    return { pass: mock.calls.length > 0, message: (not) => `expected spy ${not ? "not " : ""}to be called, called ${mock.calls.length} times` };
  },
  toHaveBeenCalledOnce(received) {
    const mock = requireMock(received, "toHaveBeenCalledOnce");
    return { pass: mock.calls.length === 1, message: (not) => `expected spy ${not ? "not " : ""}to be called once, called ${mock.calls.length} times` };
  },
  toHaveBeenCalledTimes(received, n) {
    const mock = requireMock(received, "toHaveBeenCalledTimes");
    return { pass: mock.calls.length === n, message: (not) => `expected spy ${not ? "not " : ""}to be called ${n} times, called ${mock.calls.length} times` };
  },
  toHaveBeenCalledWith(received, ...args) {
    const mock = requireMock(received, "toHaveBeenCalledWith");
    return { pass: mock.calls.some((call) => equals(call, args)), message: (not) => `expected spy ${not ? "not " : ""}to be called with ${fmt(args)}; calls: ${fmt(mock.calls)}` };
  },
  toHaveBeenLastCalledWith(received, ...args) {
    const mock = requireMock(received, "toHaveBeenLastCalledWith");
    return { pass: mock.calls.length > 0 && equals(mock.calls.at(-1), args), message: (not) => `expected last call ${not ? "not " : ""}to be ${fmt(args)}; calls: ${fmt(mock.calls)}` };
  },
  toHaveBeenNthCalledWith(received, n, ...args) {
    const mock = requireMock(received, "toHaveBeenNthCalledWith");
    return { pass: mock.calls.length >= n && equals(mock.calls[n - 1], args), message: (not) => `expected call ${n} ${not ? "not " : ""}to be ${fmt(args)}; calls: ${fmt(mock.calls)}` };
  },
  toHaveReturned(received) {
    const mock = requireMock(received, "toHaveReturned");
    return { pass: mock.results.some((r) => r.type === "return"), message: (not) => `expected spy ${not ? "not " : ""}to have returned` };
  },
  toHaveReturnedTimes(received, n) {
    const mock = requireMock(received, "toHaveReturnedTimes");
    const count = mock.results.filter((r) => r.type === "return").length;
    return { pass: count === n, message: (not) => `expected spy ${not ? "not " : ""}to have returned ${n} times, returned ${count} times` };
  },
  toHaveReturnedWith(received, value) {
    const mock = requireMock(received, "toHaveReturnedWith");
    return { pass: mock.results.some((r) => r.type === "return" && equals(r.value, value)), message: (not) => `expected spy ${not ? "not " : ""}to have returned ${fmt(value)}` };
  },
};
matchers.toThrowError = matchers.toThrow;
matchers.toBeCalled = matchers.toHaveBeenCalled;
matchers.toBeCalledTimes = matchers.toHaveBeenCalledTimes;
matchers.toBeCalledWith = matchers.toHaveBeenCalledWith;

class Assertion {
  constructor(actual, isNot, mode, message) {
    this.actual = actual;
    this.isNot = isNot;
    this.mode = mode;
    this.message = message;
  }
  get not() {
    return new Assertion(this.actual, !this.isNot, this.mode, this.message);
  }
  get resolves() {
    return new Assertion(this.actual, this.isNot, "resolves", this.message);
  }
  get rejects() {
    return new Assertion(this.actual, this.isNot, "rejects", this.message);
  }
}

function invokeMatcher(assertion, name, matcher, args) {
  assertionCalls++;
  const finish = (result) => {
    if (result.pass === assertion.isNot) {
      const text = typeof result.message === "function" ? result.message(assertion.isNot) : String(result.message ?? `${name} failed`);
      throw new AssertionError(assertion.message ? `${assertion.message}: ${text}` : text, assertion.actual, args[0]);
    }
  };
  const check = (actual) => {
    const result = matcher.call({ isNot: assertion.isNot, mode: assertion.mode, equals }, actual, ...args);
    if (result && typeof result.then === "function") return result.then(finish);
    return finish(result);
  };
  if (assertion.mode === "resolves" || assertion.mode === "rejects") {
    const promise = typeof assertion.actual === "function" ? assertion.actual() : assertion.actual;
    if (!promise || typeof promise.then !== "function") {
      throw new AssertionError(`expected ${fmt(promise)} to be a promise (.${assertion.mode})`);
    }
    return Promise.resolve(promise).then(
      (value) => {
        if (assertion.mode === "rejects") throw new AssertionError(`promise resolved "${fmt(value)}" instead of rejecting`);
        return check(value);
      },
      (error) => {
        if (assertion.mode === "resolves") throw new AssertionError(`promise rejected "${fmt(error)}" instead of resolving`);
        return check(error);
      },
    );
  }
  return check(assertion.actual);
}

function defineMatcher(name, matcher) {
  matchers[name] = matcher;
  Object.defineProperty(Assertion.prototype, name, {
    configurable: true,
    writable: true,
    value: function (...args) {
      return invokeMatcher(this, name, matcher, args);
    },
  });
}
for (const [name, matcher] of Object.entries(matchers)) defineMatcher(name, matcher);

export function expect(actual, message) {
  return new Assertion(actual, false, undefined, message);
}
expect.any = (ctor) => new Any(ctor);
expect.anything = () => new Anything();
expect.objectContaining = (sample) => new ObjectContaining(sample);
expect.arrayContaining = (sample) => new ArrayContaining(sample);
expect.stringContaining = (sample) => new StringContaining(sample);
expect.stringMatching = (sample) => new StringMatching(sample);
expect.closeTo = (sample, precision) => new CloseTo(sample, precision);
expect.toSatisfy = (predicate) => new Satisfies(predicate);
expect.not = {
  objectContaining: (sample) => new ObjectContaining(sample, true),
  arrayContaining: (sample) => new ArrayContaining(sample, true),
  stringContaining: (sample) => new StringContaining(sample, true),
  stringMatching: (sample) => new StringMatching(sample, true),
  closeTo: (sample, precision) => new CloseTo(sample, precision, true),
};
expect.unreachable = (message) => {
  throw new AssertionError(`expected${message ? ` "${message}"` : ""} not to be reached`);
};
expect.assertions = (n) => {
  expectedAssertions = n;
};
expect.hasAssertions = () => {
  expectHasAssertions = true;
};
expect.extend = (extensions) => {
  for (const [name, fn] of Object.entries(extensions)) {
    defineMatcher(name, function (received, ...args) {
      const result = fn.call(this, received, ...args);
      const normalize = (r) => ({ pass: r.pass, message: () => (typeof r.message === "function" ? r.message() : r.message) });
      return result && typeof result.then === "function" ? result.then(normalize) : normalize(result);
    });
  }
};
expect.getState = () => ({ assertionCalls, currentTestName: currentTest?.fullName });
expect.soft = () => {
  throw new Error("expect.soft is not supported by the embedded vitest shim");
};

const typeAssertion = new Proxy(function () {}, {
  get: (_target, key) => (key === "then" ? undefined : typeAssertion),
  apply: () => typeAssertion,
});
/** Type assertions are compile-time only: every member is a no-op. */
export const expectTypeOf = () => typeAssertion;
export const assertType = () => {};

// ---------------------------------------------------------------------------
// Mocks
// ---------------------------------------------------------------------------

const allMocks = new Set();
let invocationOrder = 0;

function createMock(implementation, restore) {
  const original = implementation;
  let impl = implementation;
  let once = [];
  let name = "vi.fn()";
  const state = {
    calls: [],
    results: [],
    settledResults: [],
    instances: [],
    contexts: [],
    invocationCallOrder: [],
    get lastCall() {
      return this.calls.at(-1);
    },
  };
  function mock(...args) {
    state.calls.push(args);
    state.contexts.push(this);
    state.invocationCallOrder.push(++invocationOrder);
    const result = { type: "incomplete", value: undefined };
    state.results.push(result);
    const settled = { type: "incomplete", value: undefined };
    state.settledResults.push(settled);
    const fn = once.length > 0 ? once.shift() : impl;
    try {
      let value;
      if (new.target) {
        value = fn === undefined ? this : Reflect.construct(fn, args, new.target);
        state.instances.push(value);
      } else {
        state.instances.push(this);
        value = fn === undefined ? undefined : fn.apply(this, args);
      }
      result.type = "return";
      result.value = value;
      if (value !== null && typeof value === "object" && typeof value.then === "function") {
        value.then(
          (v) => {
            settled.type = "fulfilled";
            settled.value = v;
          },
          (e) => {
            settled.type = "rejected";
            settled.value = e;
          },
        );
      } else {
        settled.type = "fulfilled";
        settled.value = value;
      }
      return value;
    } catch (error) {
      result.type = "throw";
      result.value = error;
      settled.type = "rejected";
      settled.value = error;
      throw error;
    }
  }
  const clear = () => {
    for (const key of ["calls", "results", "settledResults", "instances", "contexts", "invocationCallOrder"]) state[key] = [];
  };
  Object.assign(mock, {
    _isMockFunction: true,
    mock: state,
    getMockName: () => name,
    mockName(n) {
      name = n;
      return mock;
    },
    getMockImplementation: () => impl,
    mockImplementation(fn) {
      impl = fn;
      return mock;
    },
    mockImplementationOnce(fn) {
      once.push(fn);
      return mock;
    },
    mockReturnValue: (v) => mock.mockImplementation(() => v),
    mockReturnValueOnce: (v) => mock.mockImplementationOnce(() => v),
    mockResolvedValue: (v) => mock.mockImplementation(() => Promise.resolve(v)),
    mockResolvedValueOnce: (v) => mock.mockImplementationOnce(() => Promise.resolve(v)),
    mockRejectedValue: (e) => mock.mockImplementation(() => Promise.reject(e)),
    mockRejectedValueOnce: (e) => mock.mockImplementationOnce(() => Promise.reject(e)),
    mockReturnThis: () =>
      mock.mockImplementation(function () {
        return this;
      }),
    withImplementation(fn, callback) {
      const previous = impl;
      impl = fn;
      const result = callback();
      if (result && typeof result.then === "function") return result.finally(() => (impl = previous));
      impl = previous;
      return result;
    },
    mockClear() {
      clear();
      return mock;
    },
    mockReset() {
      clear();
      impl = original;
      once = [];
      return mock;
    },
    mockRestore() {
      mock.mockReset();
      restore?.();
      return mock;
    },
  });
  Object.defineProperty(mock, "length", { value: original?.length ?? 0 });
  allMocks.add(mock);
  if (restore) mock._isSpy = true;
  return mock;
}

function findDescriptor(object, key) {
  for (let o = object; o !== null && o !== undefined; o = Object.getPrototypeOf(o)) {
    const d = Object.getOwnPropertyDescriptor(o, key);
    if (d) return d;
  }
  return undefined;
}

function spyOn(object, key, accessType) {
  if (object === null || (typeof object !== "object" && typeof object !== "function")) throw new TypeError(`vi.spyOn() could not spy on a non-object ${fmt(object)}`);
  const descriptor = findDescriptor(object, key);
  if (!descriptor) throw new Error(`${String(key)} does not exist`);
  const own = Object.getOwnPropertyDescriptor(object, key);
  const restore = () => {
    if (own) Object.defineProperty(object, key, own);
    else delete object[key];
  };
  if (accessType === "get" || accessType === "set") {
    const accessor = descriptor[accessType];
    if (isMock(accessor)) return accessor;
    const spy = createMock(function (...args) {
      return accessor?.apply(this, args);
    }, restore);
    Object.defineProperty(object, key, { configurable: true, enumerable: descriptor.enumerable, get: descriptor.get, set: descriptor.set, [accessType]: spy });
    return spy;
  }
  const original = descriptor.value ?? descriptor.get?.call(object);
  if (isMock(original)) return original;
  if (typeof original !== "function") throw new TypeError(`${String(key)} is not a function`);
  const spy = createMock(function (...args) {
    return new.target ? Reflect.construct(original, args, new.target) : original.apply(this, args);
  }, restore);
  Object.defineProperty(spy, "name", { value: original.name });
  Object.defineProperty(object, key, { configurable: true, enumerable: descriptor.enumerable ?? false, writable: true, value: spy });
  return spy;
}

// ---------------------------------------------------------------------------
// Fake timers
// ---------------------------------------------------------------------------

let fake = null;
let mockedDate;

function dateNow() {
  if (fake) return T.now() + fake.dateOffset;
  if (mockedDate !== undefined) return mockedDate;
  return realDateNow();
}

function makeFakeDate() {
  function FakeDate(...args) {
    if (!new.target) return new RealDate(dateNow()).toString();
    return Reflect.construct(RealDate, args.length === 0 ? [dateNow()] : args, new.target);
  }
  Object.setPrototypeOf(FakeDate, RealDate);
  FakeDate.prototype = RealDate.prototype;
  FakeDate.now = () => dateNow();
  FakeDate.parse = RealDate.parse;
  FakeDate.UTC = RealDate.UTC;
  return FakeDate;
}

const patched = [];
function patch(object, key, value) {
  if (!object) return;
  patched.push([object, key, Object.getOwnPropertyDescriptor(object, key)]);
  Object.defineProperty(object, key, { configurable: true, writable: true, enumerable: false, value });
}
function unpatchAll() {
  while (patched.length > 0) {
    const [object, key, descriptor] = patched.pop();
    if (descriptor) Object.defineProperty(object, key, descriptor);
    else delete object[key];
  }
}

function installDate() {
  if (globalThis.Date === RealDate) patch(globalThis, "Date", makeFakeDate());
}

function toMs(value) {
  if (value === undefined) return realDateNow();
  if (value instanceof RealDate) return value.getTime();
  return Number(value instanceof Date ? value.getTime() : new RealDate(value).getTime());
}

function requireTimers() {
  if (!T) throw new Error("vi.useFakeTimers() needs a runtime with RuntimeOptions::timer_control");
}

function useFakeTimers(config = {}) {
  requireTimers();
  const start = config.now === undefined ? dateNow() : toMs(config.now);
  if (fake) useRealTimers();
  mockedDate = undefined;
  T.freeze(start);
  fake = { dateOffset: 0, perfStart: performance.now(), clockStart: start };
  unpatchAll();
  installDate();
  const perfNow = () => fake.perfStart + (T.now() - fake.clockStart);
  patch(performance, "now", perfNow);
  if (globalThis.process?.hrtime) {
    const hrtime = (previous) => {
      const ns = BigInt(Math.round(perfNow() * 1e6));
      let seconds = Number(ns / 1000000000n);
      let nanos = Number(ns % 1000000000n);
      if (previous) {
        seconds -= previous[0];
        nanos -= previous[1];
        if (nanos < 0) {
          seconds -= 1;
          nanos += 1e9;
        }
      }
      return [seconds, nanos];
    };
    hrtime.bigint = () => BigInt(Math.round(perfNow() * 1e6));
    patch(process, "hrtime", hrtime);
  }
  return vi;
}

function useRealTimers() {
  if (fake) T.thaw();
  fake = null;
  mockedDate = undefined;
  unpatchAll();
  return vi;
}

function setSystemTime(time) {
  const ms = toMs(time);
  if (fake) fake.dateOffset = ms - T.now();
  else {
    mockedDate = ms;
    installDate();
  }
  return vi;
}

function requireFake(name) {
  if (!fake) throw new Error(`Timers are not mocked. Try calling "vi.useFakeTimers()" first. (${name})`);
}

const LOOP_LIMIT = 10000;

const timers = {
  useFakeTimers,
  useRealTimers,
  setSystemTime,
  isFakeTimers: () => fake !== null,
  getMockedSystemTime: () => (fake ? new RealDate(dateNow()) : mockedDate === undefined ? null : new RealDate(mockedDate)),
  getRealSystemTime: () => realDateNow(),
  getTimerCount() {
    return fake ? T.count() : 0;
  },
  clearAllTimers() {
    if (fake) T.freeze(T.now());
    return vi;
  },
  advanceTimersByTime(ms) {
    requireFake("advanceTimersByTime");
    T.advance(ms);
    return vi;
  },
  async advanceTimersByTimeAsync(ms) {
    requireFake("advanceTimersByTimeAsync");
    const target = T.now() + Math.max(0, Number(ms) || 0);
    await realYield();
    for (;;) {
      if (!fake) return vi;
      const next = T.next();
      if (next === undefined || next > target) break;
      T.runNext(target);
      await realYield();
    }
    if (fake && T.now() < target) T.setNow(target);
    return vi;
  },
  advanceTimersToNextTimer(steps = 1) {
    requireFake("advanceTimersToNextTimer");
    for (let i = 0; i < steps; i++) if (!T.runNext()) break;
    return vi;
  },
  async advanceTimersToNextTimerAsync(steps = 1) {
    requireFake("advanceTimersToNextTimerAsync");
    for (let i = 0; i < steps; i++) {
      await realYield();
      if (!fake || !T.runNext()) break;
    }
    await realYield();
    return vi;
  },
  runAllTimers() {
    requireFake("runAllTimers");
    for (let i = 0; T.runNext(); i++) {
      if (i >= LOOP_LIMIT) throw new Error(`Aborting after running ${LOOP_LIMIT} timers, assuming an infinite loop!`);
    }
    return vi;
  },
  async runAllTimersAsync() {
    requireFake("runAllTimersAsync");
    for (let i = 0; ; i++) {
      await realYield();
      if (!fake || !T.runNext()) break;
      if (i >= LOOP_LIMIT) throw new Error(`Aborting after running ${LOOP_LIMIT} timers, assuming an infinite loop!`);
    }
    return vi;
  },
  runOnlyPendingTimers() {
    requireFake("runOnlyPendingTimers");
    for (let n = T.count(); n > 0 && T.runNext(); n--);
    return vi;
  },
  async runOnlyPendingTimersAsync() {
    requireFake("runOnlyPendingTimersAsync");
    for (let n = T.count(); n > 0; n--) {
      await realYield();
      if (!fake || !T.runNext()) break;
    }
    await realYield();
    return vi;
  },
};

// ---------------------------------------------------------------------------
// vi
// ---------------------------------------------------------------------------

const stubbedEnv = new Map();
const pendingMocks = [];
const appliedMocks = [];
const mockModules = new Map();
globalThis[Symbol.for("pi-durable.vitest.mocks")] = mockModules;

/**
 * Build every registered `vi.mock` and install it (src/dispatch/vitest.rs): later imports of the
 * specifier (relative paths resolve against `baseUrl`, the test file) get the factory's result.
 * `importOriginal` resolves to the unmocked module.
 */
export async function __applyMocks(baseUrl) {
  for (const { specifier, factory } of pendingMocks.splice(0)) {
    const isPath = specifier.startsWith(".") || specifier.startsWith("/") || specifier.startsWith("file:");
    const target = isPath ? fileURLToPath(new URL(specifier, baseUrl)) : specifier;
    const bare = specifier.startsWith("node:") ? specifier.slice(5) : specifier;
    const builtin = !isPath && nodeModule.isBuiltin(bare);
    // Start loading the original before the mock is installed; a failure surfaces only if used.
    const original = import(builtin ? `node:${bare}` : target).then((v) => ({ v }), (e) => ({ e }));
    const importOriginal = async () => {
      const r = await original;
      if ("e" in r) throw r.e;
      return r.v;
    };
    const result = await factory(importOriginal);
    if (result === null || typeof result !== "object") throw new Error(`[vitest] vi.mock("${specifier}", factory?) did not return an object`);
    const exports = { ...result };
    if ("default" in exports) Object.defineProperty(exports, "__esModule", { value: true });
    const id = `${appliedMocks.length}:${specifier}`;
    mockModules.set(id, exports);
    call("vitest.mock", id, builtin ? [`node:${bare}`, bare] : [target]);
    appliedMocks.push(specifier);
  }
}
const stubbedGlobals = new Map();

async function waitFor(callback, options = {}) {
  const { timeout = 1000, interval = 50 } = typeof options === "number" ? { timeout: options } : options;
  const start = realDateNow();
  let lastError;
  for (;;) {
    if (fake) T.advance(interval);
    try {
      return await callback();
    } catch (error) {
      lastError = error;
    }
    if (realDateNow() - start >= timeout) throw lastError ?? new Error("Timed out in waitFor!");
    await new Promise((resolve) => realTimeout(resolve, interval));
  }
}

async function waitUntil(callback, options = {}) {
  const { timeout = 1000, interval = 50 } = typeof options === "number" ? { timeout: options } : options;
  const start = realDateNow();
  for (;;) {
    if (fake) T.advance(interval);
    const result = await callback();
    if (result) return result;
    if (realDateNow() - start >= timeout) throw new Error("Timed out in waitUntil!");
    await new Promise((resolve) => realTimeout(resolve, interval));
  }
}

export const vi = {
  fn: (implementation) => createMock(implementation),
  spyOn,
  isMockFunction: isMock,
  mocked: (value) => value,
  hoisted: (factory) => factory(),
  mock(specifier, factory) {
    // Registered here, installed by `__applyMocks`, which the runner's hoisting transform
    // (src/hoist.rs) calls before the test file's other imports load. A call the transform did
    // not hoist (not top-level) is reported as `unappliedMocks`.
    if (typeof factory !== "function") throw new Error(`vi.mock("${specifier}") without a factory (automocking) is not supported by the embedded vitest shim`);
    pendingMocks.push({ specifier: String(specifier), factory });
  },
  doMock() {
    throw new Error("vi.doMock() is not supported by the embedded vitest shim");
  },
  unmock(specifier) {
    const index = pendingMocks.findIndex((m) => m.specifier === String(specifier));
    if (index !== -1) pendingMocks.splice(index, 1);
  },
  doUnmock() {},
  importActual: (specifier) => import(specifier),
  clearAllMocks() {
    for (const mock of allMocks) mock.mockClear();
    return vi;
  },
  resetAllMocks() {
    for (const mock of allMocks) mock.mockReset();
    return vi;
  },
  restoreAllMocks() {
    for (const mock of allMocks) if (mock._isSpy) mock.mockRestore();
    return vi;
  },
  stubEnv(name, value) {
    if (!stubbedEnv.has(name)) stubbedEnv.set(name, hasOwn(process.env, name) ? process.env[name] : undefined);
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
    return vi;
  },
  unstubAllEnvs() {
    for (const [name, value] of stubbedEnv) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
    stubbedEnv.clear();
    return vi;
  },
  stubGlobal(name, value) {
    if (!stubbedGlobals.has(name)) stubbedGlobals.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
    Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: true });
    return vi;
  },
  unstubAllGlobals() {
    for (const [name, descriptor] of stubbedGlobals) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
    stubbedGlobals.clear();
    return vi;
  },
  waitFor,
  waitUntil,
  ...timers,
};
export const vitest = vi;

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

function withTimeout(run, ms, label, controller) {
  return new Promise((resolve, reject) => {
    let timer;
    if (Number.isFinite(ms) && ms > 0) {
      timer = realTimeout(() => {
        const error = new Error(`${label} timed out in ${ms}ms.`);
        // Like vitest: the test context's `signal` aborts when the test times out, so work the
        // test leaves running can stop before the next test starts.
        controller?.abort(error);
        reject(error);
      }, ms);
    }
    Promise.resolve()
      .then(run)
      .then(
        (value) => {
          if (timer !== undefined) realClear(timer);
          resolve(value);
        },
        (error) => {
          if (timer !== undefined) realClear(timer);
          reject(error);
        },
      );
  });
}

function errorText(error) {
  if (error instanceof Error) return error.stack && error.stack.includes(error.message) ? error.stack : `${error.name}: ${error.message}\n${error.stack ?? ""}`;
  return fmt(error);
}

function hasRunnable(node) {
  if (node.type === "test") return node.mode === "run" || node.mode === "only";
  if (node.mode === "skip" || node.mode === "todo") return false;
  return node.children.some((child) => hasRunnable(child));
}

function markOnly(node, inOnly) {
  // With `.only` anywhere, only `.only` tests and tests inside `.only` suites run.
  if (node.type === "test") {
    if (node.mode === "run" && !inOnly) node.mode = "skip";
    return;
  }
  const only = inOnly || node.mode === "only";
  for (const child of node.children) markOnly(child, only);
}

async function runHooks(suite, kind, context, cleanups) {
  const hooks = kind === "afterAll" || kind === "afterEach" ? [...suite.hooks[kind]].reverse() : suite.hooks[kind];
  for (const hook of hooks) {
    const result = await withTimeout(() => hook.fn(context), hook.timeout ?? DEFAULT_HOOK_TIMEOUT, `Hook ${kind}`);
    if (typeof result === "function" && cleanups) cleanups.push(result);
  }
}

function chain(suite) {
  const out = [];
  for (let s = suite; s; s = s.parent) out.unshift(s);
  return out;
}

const results = [];

async function runTest(test, suiteFailure, skipAll) {
  if (globalThis.process?.env?.PI_DURABLE_NATIVE_TRACE === "1") globalThis.process.stderr.write(`[test] ${test.name}\n`);
  const ancestorTitles = test.suite.titles();
  const fullName = [...ancestorTitles, test.name].join(" ");
  const entry = { ancestorTitles, title: test.name, fullName, status: "passed", duration: 0, failureMessages: [] };
  results.push(entry);
  if (skipAll || test.mode === "skip" || test.mode === "todo") {
    entry.status = test.mode === "todo" || skipAll === "todo" ? "todo" : "skipped";
    return;
  }
  if (suiteFailure !== undefined) {
    entry.status = "failed";
    entry.failureMessages.push(errorText(suiteFailure));
    return;
  }
  const started = realDateNow();
  const errors = [];
  const record = { fullName, finished: [], failed: [] };
  currentTest = record;
  assertionCalls = 0;
  expectedAssertions = undefined;
  expectHasAssertions = false;
  const controller = new AbortController();
  const context = {
    task: { name: test.name, fullName, suite: { name: test.suite.name } },
    expect,
    skip: () => {
      throw Object.assign(new Error("skipped"), { [skipMarker]: true });
    },
    onTestFinished,
    onTestFailed,
    signal: controller.signal,
  };
  const suites = chain(test.suite);
  const cleanups = [];
  let skipped = false;
  try {
    for (const suite of suites) await runHooks(suite, "beforeEach", context, cleanups);
    await withTimeout(() => test.fn(context), test.timeout ?? test.suite.timeout ?? DEFAULT_TEST_TIMEOUT, "Test", controller);
    if (expectedAssertions !== undefined && assertionCalls !== expectedAssertions) {
      throw new AssertionError(`expected number of assertions to be ${expectedAssertions}, but got ${assertionCalls}`);
    }
    if (expectHasAssertions && assertionCalls === 0) throw new AssertionError("expected any number of assertion, but got none");
  } catch (error) {
    if (error?.[skipMarker]) skipped = true;
    else errors.push(error);
  }
  for (const suite of [...suites].reverse()) {
    try {
      await runHooks(suite, "afterEach", context);
    } catch (error) {
      errors.push(error);
    }
  }
  for (const cleanup of cleanups.reverse()) {
    try {
      await withTimeout(cleanup, DEFAULT_HOOK_TIMEOUT, "Hook beforeEach cleanup");
    } catch (error) {
      errors.push(error);
    }
  }
  const failedNow = test.fails ? errors.length === 0 : errors.length > 0;
  if (failedNow) {
    for (const fn of [...record.failed].reverse()) {
      try {
        await withTimeout(() => fn(context), DEFAULT_HOOK_TIMEOUT, "onTestFailed");
      } catch (error) {
        errors.push(error);
      }
    }
  }
  for (const fn of [...record.finished].reverse()) {
    try {
      await withTimeout(() => fn(context), DEFAULT_HOOK_TIMEOUT, "onTestFinished");
    } catch (error) {
      errors.push(error);
    }
  }
  currentTest = undefined;
  entry.duration = realDateNow() - started;
  if (skipped && errors.length === 0) {
    entry.status = "skipped";
    return;
  }
  if (test.fails) {
    if (errors.length === 0) {
      entry.status = "failed";
      entry.failureMessages.push("Expect test to fail");
    }
    return;
  }
  if (errors.length > 0) {
    entry.status = "failed";
    for (const error of errors) entry.failureMessages.push(errorText(error));
  }
}

const skipMarker = Symbol("vitest.skip");

async function runSuite(suite, inheritedFailure, inheritedSkip) {
  const skipAll = inheritedSkip ?? (suite.mode === "skip" || suite.mode === "todo" ? suite.mode : undefined);
  const runnable = skipAll === undefined && hasRunnable(suite);
  let failure = inheritedFailure;
  const cleanups = [];
  if (runnable && failure === undefined) {
    try {
      await runHooks(suite, "beforeAll", {}, cleanups);
    } catch (error) {
      failure = error;
    }
  }
  for (const child of suite.children) {
    if (child.type === "test") await runTest(child, failure, skipAll);
    else await runSuite(child, failure, skipAll);
  }
  if (runnable && inheritedFailure === undefined) {
    const errors = [];
    try {
      await runHooks(suite, "afterAll", {});
    } catch (error) {
      errors.push(error);
    }
    for (const cleanup of cleanups.reverse()) {
      try {
        await withTimeout(cleanup, DEFAULT_HOOK_TIMEOUT, "Hook beforeAll cleanup");
      } catch (error) {
        errors.push(error);
      }
    }
    if (errors.length > 0) suiteErrors.push(...errors);
  }
}

const suiteErrors = [];

/** Run the collected tests; returns a vitest-JSON-like report. */
async function run({ file } = {}) {
  await Promise.all(pendingCollection);
  if (hasOnly) markOnly(root, false);
  const started = realDateNow();
  await runSuite(root, collectionErrors[0]);
  useRealTimers();
  const count = (status) => results.filter((r) => r.status === status).length;
  const failed = count("failed");
  const fileErrors = [...collectionErrors, ...suiteErrors].map(errorText);
  return {
    numTotalTests: results.length,
    numPassedTests: count("passed"),
    numFailedTests: failed,
    numPendingTests: count("skipped"),
    numTodoTests: count("todo"),
    success: failed === 0 && fileErrors.length === 0,
    startTime: started,
    appliedMocks,
    unappliedMocks: pendingMocks.map((m) => m.specifier),
    testResults: [
      {
        name: file ?? "",
        status: failed === 0 && fileErrors.length === 0 ? "passed" : "failed",
        message: fileErrors.join("\n"),
        assertionResults: results,
      },
    ],
  };
}

/** Runner entry points used by the embedded runner (not part of vitest's API). */
export const __runner = {
  run,
  /** Mock bookkeeping, readable even when the test file failed to load. */
  mocks: () => ({ appliedMocks: [...appliedMocks], unappliedMocks: pendingMocks.map((m) => m.specifier) }),
};
