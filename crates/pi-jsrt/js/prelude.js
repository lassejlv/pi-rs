// pi-jsrt prelude: internal helpers used by the Rust side, the builtin module
// registry, CommonJS module objects, Error.stack formatting and timers.
// Evaluated as `(function (ops, internal) { ... })`.

// Values of lazily loaded globals (see below); glue defines them through `hidden`.
const lazyValues = Object.create(null);
const hidden = (obj, name, value) => {
  if (obj === globalThis && typeof name === "string" && lazyGlobalNames.has(name)) {
    lazyValues[name] = value;
    return obj;
  }
  return Object.defineProperty(obj, name, { value, writable: true, configurable: true, enumerable: false });
};
const lazyGlobalNames = new Set(Object.keys(ops.config.lazyGlobals ?? {}));
internal.hidden = hidden;

// A libuv-style handle for the event loop's liveness: counted while active and referenced.
internal.loopHandle = () => {
  let active = false;
  let refed = true;
  let counted = false;
  const update = () => {
    const want = active && refed;
    if (want !== counted) {
      counted = want;
      ops.loopRef(want ? 1 : -1);
    }
  };
  return {
    activate() {
      active = true;
      update();
    },
    deactivate() {
      active = false;
      update();
    },
    ref() {
      refed = true;
      update();
    },
    unref() {
      refed = false;
      update();
    },
    hasRef: () => refed,
  };
};

// Node's `determineSpecificType` for "Received ..." in ERR_INVALID_ARG_TYPE messages.
internal.describeReceived = (value) => {
  if (value == null) return "" + value;
  if (typeof value === "function" && value.name) return `function ${value.name}`;
  if (typeof value === "object") {
    if (value.constructor?.name) return `an instance of ${value.constructor.name}`;
    return internal.inspect ? internal.inspect(value, { depth: -1 }) : "an object";
  }
  let inspected = internal.inspect ? internal.inspect(value, { colors: false }) : String(value);
  if (inspected.length > 28) inspected = `${inspected.slice(0, 25)}...`;
  return `type ${typeof value} (${inspected})`;
};

// ---------------------------------------------------------------------------
// ArrayBuffer.prototype.transfer / transferToFixedLength / detached (ES2024), for engines without
// them (QuickJS-ng has them; vm realms share this shim)
// ---------------------------------------------------------------------------

if (typeof ArrayBuffer.prototype.transfer !== "function") {
  const define = (name, fn) =>
    Object.defineProperty(ArrayBuffer.prototype, name, { value: fn, writable: true, configurable: true, enumerable: false });
  define("transfer", {
    transfer(newLength) {
      return ops.arrayBufferTransfer(this, newLength, this.resizable ? this.maxByteLength : undefined);
    },
  }.transfer);
  define("transferToFixedLength", {
    transferToFixedLength(newLength) {
      return ops.arrayBufferTransfer(this, newLength, undefined);
    },
  }.transferToFixedLength);
  Object.defineProperty(ArrayBuffer.prototype, "detached", {
    get: { get detached() { return ops.arrayBufferDetached(this); } }.__lookupGetter__?.("detached") ?? function () {
      return ops.arrayBufferDetached(this);
    },
    configurable: true,
    enumerable: false,
  });
}

// ---------------------------------------------------------------------------
// JSON conversion helpers (see src/convert.rs)
// ---------------------------------------------------------------------------

internal.handleMarker = (value) => ({ $jsHandle: ops.handleNew(value) });

function toHostReplacer(key, value) {
  switch (typeof value) {
    case "function":
      return { $jsHandle: ops.handleNew(value) };
    case "bigint":
      return value.toString();
    case "symbol":
      return undefined;
    case "object":
      if (value === null) return null;
      if (value instanceof Error) {
        const out = { name: value.name, message: value.message };
        if (typeof value.stack === "string") out.stack = value.stack;
        for (const k of Object.keys(value)) out[k] = value[k];
        if (value.cause !== undefined) out.cause = value.cause;
        return out;
      }
      if (ArrayBuffer.isView(value) && !(value instanceof DataView)) return Array.from(value);
      if (value instanceof ArrayBuffer) return Array.from(new Uint8Array(value));
      if (value instanceof Map) {
        const out = {};
        for (const [k, v] of value) out[String(k)] = v;
        return out;
      }
      if (value instanceof Set) return Array.from(value);
      return value;
    default:
      return value;
  }
}

internal.toHost = function toHost(value) {
  if (value === undefined) return undefined;
  return JSON.stringify(value, toHostReplacer);
};

function fromHostReviver(key, value) {
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    if (typeof value.$jsHandle === "number") return ops.handleGet(value.$jsHandle);
    if (typeof value.$hostFn === "number") return ops.hostFnGet(value.$hostFn);
  }
  return value;
}

internal.fromHost = (text) => JSON.parse(text, fromHostReviver);

// ---------------------------------------------------------------------------
// Builtin module registry
// ---------------------------------------------------------------------------

const builtinFactories = new Map();
const builtinCache = new Map();
const lazyBuiltins = ops.config.lazyBuiltins ?? Object.create(null);
const lazyGlobals = ops.config.lazyGlobals ?? Object.create(null);
const loadedGlue = new Set();
internal.loadGlue = (file) => {
  if (loadedGlue.has(file)) return;
  loadedGlue.add(file);
  ops.loadGlue(file);
};
internal.defineBuiltin = (name, factory) => {
  builtinFactories.set(name, factory);
};
internal.hasBuiltin = (name) => builtinFactories.has(name) || Object.prototype.hasOwnProperty.call(lazyBuiltins, name);
internal.builtinNames = () => Array.from(new Set([...builtinFactories.keys(), ...Object.keys(lazyBuiltins)]));
internal.requireBuiltin = (name) => {
  if (builtinCache.has(name)) return builtinCache.get(name);
  if (!builtinFactories.has(name) && Object.prototype.hasOwnProperty.call(lazyBuiltins, name)) {
    internal.loadGlue(lazyBuiltins[name]);
  }
  const factory = builtinFactories.get(name);
  if (!factory) {
    const err = new Error(`Cannot find module '${name}'`);
    err.code = "MODULE_NOT_FOUND";
    throw err;
  }
  const exports = factory();
  builtinCache.set(name, exports);
  return exports;
};

// Globals provided by lazily loaded glue: an accessor loads the file on first use.
// Lazy globals stay accessors for good, backed by `lazyValues` (redefining a global object
// property from accessor to data inside its own getter broke the previous engine's inline caches;
// accessors also keep `globalThis.X = v` working before the first read).
for (const name of Object.keys(lazyGlobals)) {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    enumerable: false,
    get() {
      if (!(name in lazyValues)) internal.loadGlue(lazyGlobals[name]);
      return lazyValues[name];
    },
    set(value) {
      lazyValues[name] = value;
    },
  });
}

// ---------------------------------------------------------------------------
// CommonJS
// ---------------------------------------------------------------------------

const requireCache = Object.create(null);

function dirnameOf(filename) {
  const i = filename.lastIndexOf("/");
  if (i < 0) return ".";
  if (i === 0) return "/";
  return filename.slice(0, i);
}

function makeRequire(filename) {
  const require = function require(id) {
    if (typeof id !== "string") {
      throw new TypeError(`The "id" argument must be of type string. Received ${typeof id}`);
    }
    if (id === "") throw new TypeError("The argument 'id' must be a non-empty string. Received ''");
    return ops.require(id, filename);
  };
  const resolve = function resolve(id) {
    return ops.requireResolve(String(id), filename, false);
  };
  resolve.paths = (id) => {
    if (internal.hasBuiltin(String(id).replace(/^node:/, ""))) return null;
    const out = [];
    let dir = filename ? dirnameOf(filename) : ops.cwd();
    for (;;) {
      if (!dir.endsWith("/node_modules")) out.push((dir === "/" ? "" : dir) + "/node_modules");
      if (dir === "/" || dir === ".") break;
      dir = dirnameOf(dir);
    }
    return out;
  };
  require.resolve = resolve;
  require.cache = requireCache;
  // The entry module in Node mode (src/node.rs); undefined in an embedding.
  Object.defineProperty(require, "main", {
    configurable: true,
    enumerable: true,
    get: () => (internal.mainFilename ? requireCache[internal.mainFilename] : undefined),
    set(v) {
      Object.defineProperty(require, "main", { value: v, writable: true, configurable: true, enumerable: true });
    },
  });
  require.extensions = Object.create(null);
  return require;
}
internal.makeRequire = makeRequire;
hidden(globalThis, Symbol.for("pi-jsrt.createRequire"), (filename) => makeRequire(filename));

internal.createCjsModule = (filename) => {
  const module = {
    id: filename,
    path: dirnameOf(filename),
    filename,
    exports: {},
    loaded: false,
    children: [],
    paths: [],
    parent: null,
  };
  module.require = makeRequire(filename);
  requireCache[filename] = module;
  return module;
};

internal.makeImportMetaResolve = (filename) =>
  function resolve(specifier) {
    const r = ops.requireResolve(String(specifier), filename, true);
    if (r.startsWith("/")) return "file://" + encodeURI(r).replace(/[?#]/g, encodeURIComponent);
    return r;
  };

// ---------------------------------------------------------------------------
// Error.stack: Node format "Name: message\n    at ..." with TS positions
// ---------------------------------------------------------------------------

{
  const desc = Object.getOwnPropertyDescriptor(Error.prototype, "stack");
  if (desc && typeof desc.get === "function") {
    const originalGet = desc.get;
    const header = (err) => {
      let name, message;
      try {
        name = err.name === undefined ? "Error" : String(err.name);
        message = err.message === undefined ? "" : String(err.message);
      } catch {
        name = "Error";
        message = "";
      }
      if (!name) return message;
      if (!message) return name;
      return `${name}: ${message}`;
    };
    Object.defineProperty(Error.prototype, "stack", {
      configurable: true,
      enumerable: false,
      get() {
        let frames = "";
        try {
          frames = originalGet.call(this) || "";
        } catch {
          frames = "";
        }
        frames = frames.replace(/\n?\s*at\s+\(native\)\s*$/g, "").replace(/\s+$/, "");
        if (frames) frames = ops.remapStack(frames);
        return frames ? `${header(this)}\n${frames}` : header(this);
      },
      set(value) {
        hidden(this, "stack", value);
      },
    });
  }
  Error.stackTraceLimit = 10;
  Error.captureStackTrace = function captureStackTrace(target, _constructorOpt) {
    if (target === null || (typeof target !== "object" && typeof target !== "function")) {
      throw new TypeError("Invalid argument");
    }
    const frames = new Error().stack.split("\n").slice(2).join("\n");
    let head;
    try {
      head = target.message ? `${target.name ?? "Error"}: ${target.message}` : String(target.name ?? "Error");
    } catch {
      head = "Error";
    }
    hidden(target, "stack", frames ? `${head}\n${frames}` : head);
  };
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

const timerIdOf = (t) => {
  if (t === null || t === undefined) return undefined;
  if (typeof t === "object") return t._id;
  const n = Number(t);
  return Number.isFinite(n) ? n : undefined;
};

class Timeout {
  constructor(id, ms, repeat) {
    hidden(this, "_id", id);
    hidden(this, "_ms", ms);
    hidden(this, "_repeat", repeat);
    hidden(this, "_ref", true);
    hidden(this, "_destroyed", false);
  }
  ref() {
    this._ref = true;
    if (!this._destroyed) ops.timerRef(this._id, true);
    return this;
  }
  unref() {
    this._ref = false;
    if (!this._destroyed) ops.timerRef(this._id, false);
    return this;
  }
  hasRef() {
    return this._ref;
  }
  refresh() {
    if (!this._destroyed) ops.timerRefresh(this._id, this._ms);
    return this;
  }
  close() {
    clearTimeout(this);
    return this;
  }
  [Symbol.toPrimitive]() {
    return this._id;
  }
  [Symbol.dispose]() {
    clearTimeout(this);
  }
}

class Immediate {
  constructor(id) {
    hidden(this, "_id", id);
    hidden(this, "_ref", true);
  }
  ref() {
    this._ref = true;
    ops.timerRef(this._id, true);
    return this;
  }
  unref() {
    this._ref = false;
    ops.timerRef(this._id, false);
    return this;
  }
  hasRef() {
    return this._ref;
  }
  [Symbol.dispose]() {
    clearImmediate(this);
  }
}

function checkCallback(cb) {
  if (typeof cb !== "function") {
    const err = new TypeError(`The "callback" argument must be of type function. Received ${cb === null ? "null" : typeof cb}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
}

function normalizeDelay(ms) {
  ms = Number(ms);
  if (!(ms >= 1 && ms <= 2147483647)) ms = 1;
  return ms;
}

function setTimeout(callback, ms, ...args) {
  checkCallback(callback);
  const delay = normalizeDelay(ms);
  let timer;
  const id = ops.timerStart(
    () => {
      timer._destroyed = true;
      callback.apply(timer, args);
    },
    delay,
    false,
  );
  timer = new Timeout(id, delay, false);
  return timer;
}

function setInterval(callback, ms, ...args) {
  checkCallback(callback);
  const delay = normalizeDelay(ms);
  let timer;
  const id = ops.timerStart(() => callback.apply(timer, args), delay, true);
  timer = new Timeout(id, delay, true);
  return timer;
}

function clearTimeout(timer) {
  const id = timerIdOf(timer);
  if (id !== undefined) ops.timerClear(id);
  if (timer && typeof timer === "object") timer._destroyed = true;
}

function setImmediate(callback, ...args) {
  checkCallback(callback);
  const id = ops.timerStart(() => callback(...args), 0, false, "Immediate");
  return new Immediate(id);
}

function clearImmediate(immediate) {
  const id = timerIdOf(immediate);
  if (id !== undefined) ops.timerClear(id);
}

function queueMicrotask(callback) {
  checkCallback(callback);
  Promise.resolve().then(() => {
    try {
      callback();
    } catch (err) {
      ops.reportError("Uncaught exception", err);
    }
  });
}

internal.timers = { setTimeout, setInterval, clearTimeout, clearInterval: clearTimeout, setImmediate, clearImmediate, Timeout, Immediate };
for (const [name, fn] of Object.entries({
  setTimeout,
  setInterval,
  clearTimeout,
  clearInterval: clearTimeout,
  setImmediate,
  clearImmediate,
  queueMicrotask,
})) {
  hidden(globalThis, name, fn);
}

// `jsrt:host` gives glue code access to handle markers.
internal.defineBuiltin("jsrt:host", () => ({
  ref: (value) => internal.handleMarker(value),
  transpile: (code, filename) => ops.transpile(String(code), filename),
}));
