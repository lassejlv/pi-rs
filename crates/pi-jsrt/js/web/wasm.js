// The WebAssembly JS API over ops/wasm.rs (wasmi). Loaded on first access to `WebAssembly`.

const kHandle = Symbol("kHandle");
const wrappers = new Map(); // handle key -> Memory/Table/Global object

class CompileError extends Error {
  constructor(message) {
    super(message);
    this.name = "CompileError";
  }
}
class LinkError extends Error {
  constructor(message) {
    super(message);
    this.name = "LinkError";
  }
}
class RuntimeError extends Error {
  constructor(message) {
    super(message);
    this.name = "RuntimeError";
  }
}
internal.wasmErrors = { CompileError, LinkError, RuntimeError };

function bufferSource(bytes, where) {
  if (ArrayBuffer.isView(bytes)) return new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (bytes instanceof ArrayBuffer) return new Uint8Array(bytes);
  throw new TypeError(`${where}: Argument 0 must be a buffer source`);
}

function toPages(value, name, where) {
  const n = Number(value);
  if (!Number.isFinite(n) || n < 0 || n > 65536) {
    throw new TypeError(`${where}: Property '${name}': value ${value} is above the upper bound 65536`);
  }
  return Math.floor(n);
}

function wrap(Cls, handle) {
  const key = ops.wasmHandleKey(handle);
  let w = wrappers.get(key);
  if (!w) {
    w = Object.create(Cls.prototype);
    Object.defineProperty(w, kHandle, { value: handle });
    wrappers.set(key, w);
  }
  return w;
}

function handleOf(value, Cls) {
  return value instanceof Cls ? value[kHandle] : undefined;
}

class Module {
  constructor(bytes) {
    if (new.target === undefined) throw new TypeError("WebAssembly.Module must be invoked with 'new'");
    Object.defineProperty(this, kHandle, { value: ops.wasmCompile(bufferSource(bytes, "WebAssembly.Module()")) });
  }
  static imports(module) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Module.imports(): Argument 0 must be a WebAssembly.Module");
    return ops.wasmModuleInfo(module[kHandle], "imports").map((i) => ({ module: i.module, name: i.name, kind: i.kind }));
  }
  static exports(module) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Module.exports(): Argument 0 must be a WebAssembly.Module");
    return ops.wasmModuleInfo(module[kHandle], "exports").map((e) => ({ name: e.name, kind: e.kind }));
  }
  static customSections(module, sectionName) {
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Module.customSections(): Argument 0 must be a WebAssembly.Module");
    return ops.wasmCustomSections(module[kHandle], String(sectionName));
  }
  get [Symbol.toStringTag]() {
    return "WebAssembly.Module";
  }
}

class Instance {
  constructor(module, importObject) {
    if (new.target === undefined) throw new TypeError("WebAssembly.Instance must be invoked with 'new'");
    if (!(module instanceof Module)) throw new TypeError("WebAssembly.Instance(): Argument 0 must be a WebAssembly.Module");
    const imports = Module.imports(module);
    if (imports.length > 0 && (importObject === undefined || importObject === null || typeof importObject !== "object")) {
      throw new TypeError("WebAssembly.Instance(): Imports argument must be present and must be an object");
    }
    const values = imports.map((imp, index) => {
      const ns = importObject[imp.module];
      if (ns === undefined || ns === null || (typeof ns !== "object" && typeof ns !== "function")) {
        throw new TypeError(`WebAssembly.Instance(): Import #${index} "${imp.module}": module is not an object or function`);
      }
      const v = ns[imp.name];
      switch (imp.kind) {
        case "memory":
          return handleOf(v, Memory);
        case "table":
          return handleOf(v, Table);
        case "global":
          return v instanceof Global ? v[kHandle] : v;
        default:
          return v;
      }
    });
    const exports = Object.create(null);
    for (const [name, kind, value] of ops.wasmInstantiate(module[kHandle], values)) {
      exports[name] = kind === "memory" ? wrap(Memory, value) : kind === "table" ? wrap(Table, value) : kind === "global" ? wrap(Global, value) : value;
    }
    Object.freeze(exports);
    Object.defineProperty(this, "exports", { value: exports, enumerable: true });
  }
  get [Symbol.toStringTag]() {
    return "WebAssembly.Instance";
  }
}

class Memory {
  constructor(descriptor) {
    if (new.target === undefined) throw new TypeError("WebAssembly.Memory must be invoked with 'new'");
    if (descriptor === null || typeof descriptor !== "object") throw new TypeError("WebAssembly.Memory(): Argument 0 must be a memory descriptor");
    const initial = toPages(descriptor.initial ?? descriptor.minimum, "initial", "WebAssembly.Memory()");
    const maximum = descriptor.maximum === undefined ? undefined : toPages(descriptor.maximum, "maximum", "WebAssembly.Memory()");
    if (maximum !== undefined && maximum < initial) {
      throw new RangeError("WebAssembly.Memory(): Property 'maximum': value " + maximum + " is below the lower bound " + initial);
    }
    const handle = ops.wasmMemoryNew(initial, maximum);
    Object.defineProperty(this, kHandle, { value: handle });
    wrappers.set(ops.wasmHandleKey(handle), this);
  }
  get buffer() {
    return ops.wasmMemory(this[kHandle], "buffer");
  }
  grow(delta) {
    return ops.wasmMemory(this[kHandle], "grow", toPages(delta, "delta", "WebAssembly.Memory.grow()"));
  }
  type() {
    return ops.wasmMemory(this[kHandle], "type");
  }
  get [Symbol.toStringTag]() {
    return "WebAssembly.Memory";
  }
}

class Table {
  constructor(descriptor, value) {
    if (new.target === undefined) throw new TypeError("WebAssembly.Table must be invoked with 'new'");
    if (descriptor === null || typeof descriptor !== "object") throw new TypeError("WebAssembly.Table(): Argument 0 must be a table descriptor");
    const initial = Number(descriptor.initial ?? descriptor.minimum ?? 0);
    const handle = ops.wasmTableNew(String(descriptor.element), initial, descriptor.maximum, value);
    Object.defineProperty(this, kHandle, { value: handle });
    wrappers.set(ops.wasmHandleKey(handle), this);
  }
  get length() {
    return ops.wasmTable(this[kHandle], "length");
  }
  get(index) {
    return ops.wasmTable(this[kHandle], "get", Number(index));
  }
  set(index, value) {
    ops.wasmTable(this[kHandle], "set", Number(index), arguments.length < 2 && ops.wasmTable(this[kHandle], "element") === "anyfunc" ? null : value);
  }
  grow(delta, value) {
    return ops.wasmTable(this[kHandle], "grow", Number(delta), value);
  }
  type() {
    return { element: ops.wasmTable(this[kHandle], "element"), minimum: this.length };
  }
  get [Symbol.toStringTag]() {
    return "WebAssembly.Table";
  }
}

class Global {
  constructor(descriptor, value) {
    if (new.target === undefined) throw new TypeError("WebAssembly.Global must be invoked with 'new'");
    if (descriptor === null || typeof descriptor !== "object") throw new TypeError("WebAssembly.Global(): Argument 0 must be a global descriptor");
    const handle = ops.wasmGlobalNew(String(descriptor.value), !!descriptor.mutable, value);
    Object.defineProperty(this, kHandle, { value: handle });
    wrappers.set(ops.wasmHandleKey(handle), this);
  }
  get value() {
    return ops.wasmGlobal(this[kHandle], "get");
  }
  set value(v) {
    ops.wasmGlobal(this[kHandle], "set", v);
  }
  valueOf() {
    return this.value;
  }
  type() {
    return ops.wasmGlobal(this[kHandle], "type");
  }
  get [Symbol.toStringTag]() {
    return "WebAssembly.Global";
  }
}

const asyncify = (fn) => (...args) => new Promise((resolve, reject) => {
  // Compile on a later turn, like engines that compile in the background.
  queueMicrotask(() => {
    try {
      resolve(fn(...args));
    } catch (err) {
      reject(err);
    }
  });
});

const compile = asyncify((bytes) => new Module(bytes));

function instantiate(source, importObject) {
  if (source instanceof Module) return asyncify(() => new Instance(source, importObject))();
  return asyncify(() => {
    const module = new Module(source);
    return { module, instance: new Instance(module, importObject) };
  })();
}

async function responseBytes(source) {
  const response = await source;
  if (!response || typeof response.arrayBuffer !== "function") {
    throw new TypeError("WebAssembly.compileStreaming(): Argument 0 must be a Response or a promise resolving to one");
  }
  if (response.ok === false) throw new TypeError(`WebAssembly.compileStreaming(): HTTP status code is not ok`);
  return response.arrayBuffer();
}

const WebAssembly = {
  Module,
  Instance,
  Memory,
  Table,
  Global,
  CompileError,
  LinkError,
  RuntimeError,
  compile,
  instantiate,
  validate(bytes) {
    return ops.wasmValidate(bufferSource(bytes, "WebAssembly.validate()"));
  },
  async compileStreaming(source) {
    return new Module(await responseBytes(source));
  },
  async instantiateStreaming(source, importObject) {
    const module = new Module(await responseBytes(source));
    return { module, instance: new Instance(module, importObject) };
  },
};
Object.defineProperty(WebAssembly, Symbol.toStringTag, { value: "WebAssembly" });
globalThis.WebAssembly = WebAssembly;
