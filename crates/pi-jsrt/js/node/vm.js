// node:vm — contexts are Boa realms (src/ops/vm.rs). A contextified object is the realm's view
// of globals: names resolve on it first, then on the realm's builtins; assignments and
// declarations go to it, as with V8's global proxy interceptors.

const kRealm = Symbol("vm.realm");
const contexts = new WeakMap(); // contextified object -> realm handle
const DONT_CONTEXTIFY = Symbol("vm_dont_contextify");
const USE_MAIN_CONTEXT_DEFAULT_LOADER = Symbol("vm_dynamic_import_main_context_default");
const hasOwn = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

function invalidArgType(name, expected, value) {
  return codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be ${expected}. Received ${internal.describeReceived(value)}`);
}

// The global object of a contextified realm: a Proxy over `target` (the realm's own global
// properties: builtins, and declarations mirrored for the Proxy invariants) and `sandbox`.
function makeGlobal(sandbox) {
  const target = {};
  const state = { init: true };
  const handler = {
    get(t, k, receiver) {
      if (!state.init && k in sandbox) return Reflect.get(sandbox, k);
      return Reflect.get(t, k, receiver === proxy ? t : receiver);
    },
    set(t, k, v, receiver) {
      if (state.init) return Reflect.set(t, k, v, t);
      const td = Reflect.getOwnPropertyDescriptor(t, k);
      if (td && !td.configurable && !hasOwn(sandbox, k) && "value" in td && !td.writable) return false;
      Reflect.set(sandbox, k, v);
      // Declared globals (non-configurable on the target) mirror their value.
      if (td && !td.configurable && "value" in td && td.writable) Reflect.defineProperty(t, k, { value: v });
      return true;
    },
    has(t, k) {
      return (!state.init && k in sandbox) || Reflect.has(t, k);
    },
    getOwnPropertyDescriptor(t, k) {
      const td = Reflect.getOwnPropertyDescriptor(t, k);
      if (!state.init && hasOwn(sandbox, k)) {
        const sd = Reflect.getOwnPropertyDescriptor(sandbox, k);
        if (td && !td.configurable) {
          if ("value" in td && td.writable && "value" in sd) return { ...td, value: sd.value };
          return td;
        }
        return { ...sd, configurable: true };
      }
      return td;
    },
    defineProperty(t, k, desc) {
      if (state.init) return Reflect.defineProperty(t, k, desc);
      // Global declarations are non-configurable: keep them on the target for the Proxy
      // invariants; the contextified object gets the binding too (V8 defines it there).
      if (desc.configurable === false && !Reflect.defineProperty(t, k, desc)) return false;
      const ok = Reflect.defineProperty(sandbox, k, desc.configurable === false ? { ...desc, configurable: true } : desc);
      return ok || desc.configurable === false;
    },
    deleteProperty(t, k) {
      const td = Reflect.getOwnPropertyDescriptor(t, k);
      if (td && !td.configurable) return false;
      if (td) Reflect.deleteProperty(t, k);
      return Reflect.deleteProperty(sandbox, k);
    },
    ownKeys(t) {
      const keys = Reflect.ownKeys(t);
      if (!state.init) for (const k of Reflect.ownKeys(sandbox)) if (!keys.includes(k)) keys.push(k);
      return keys;
    },
  };
  const proxy = new Proxy(target, handler);
  return { proxy, target, state };
}

function createContext(contextObject = {}, options = {}) {
  if (contextObject === DONT_CONTEXTIFY) {
    const realm = ops.vmCreateRealm();
    const global = ops.vmGlobal(realm);
    contexts.set(global, realm);
    return global;
  }
  if (contextObject === null || (typeof contextObject !== "object" && typeof contextObject !== "function")) {
    throw invalidArgType("contextObject", "of type object", contextObject);
  }
  if (contexts.has(contextObject)) return contextObject;
  if (options !== undefined && (options === null || typeof options !== "object")) throw invalidArgType("options", "of type object", options);
  const { proxy, target, state } = makeGlobal(contextObject);
  if (internal.exposeGcInContexts === true && typeof globalThis.gc === "function") target.gc = globalThis.gc;
  const realm = ops.vmCreateRealm(proxy, target);
  state.init = false;
  contexts.set(contextObject, realm);
  return contextObject;
}

function isContext(object) {
  if (object === null || (typeof object !== "object" && typeof object !== "function")) throw invalidArgType("object", "of type object", object);
  return contexts.has(object);
}

function realmOf(contextifiedObject) {
  if (contextifiedObject === null || typeof contextifiedObject !== "object" || !contexts.has(contextifiedObject)) {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "contextifiedObject" argument must be an vm.Context. Received ${internal.describeReceived(contextifiedObject)}`);
  }
  return contexts.get(contextifiedObject);
}

function normalizeOptions(options) {
  if (typeof options === "string") return { filename: options };
  if (options === undefined || options === null) return {};
  if (typeof options !== "object") throw invalidArgType("options", "of type object", options);
  return options;
}

class Script {
  #code;
  #filename;
  #lineOffset;
  #columnOffset;
  constructor(code, options = {}) {
    code = `${code}`;
    options = normalizeOptions(options);
    this.#code = code;
    this.#filename = options.filename === undefined ? "evalmachine.<anonymous>" : String(options.filename);
    this.#lineOffset = options.lineOffset ?? 0;
    this.#columnOffset = options.columnOffset ?? 0;
    // Compile now so syntax errors are thrown by the constructor.
    ops.vmCompile(code, this.#filename);
    if (options.produceCachedData) this.cachedDataProduced = false;
    if (options.cachedData !== undefined) this.cachedDataRejected = true;
    this.sourceMapURL = /\/\/[#@] sourceMappingURL=([^\s'"]+)\s*$/.exec(code)?.[1];
  }
  #run(realm) {
    return ops.vmRun(realm, this.#code, this.#filename, this.#lineOffset, this.#columnOffset);
  }
  runInThisContext(options) {
    return this.#run(undefined);
  }
  runInContext(contextifiedObject, options) {
    return this.#run(realmOf(contextifiedObject));
  }
  runInNewContext(contextObject, options) {
    const context = createContext(contextObject ?? {}, options);
    return this.runInContext(context, options);
  }
  createCachedData() {
    return internal.Buffer.alloc(0);
  }
}

function runInContext(code, contextifiedObject, options) {
  options = normalizeOptions(options);
  const realm = realmOf(contextifiedObject);
  return ops.vmRun(realm, `${code}`, options.filename ?? "evalmachine.<anonymous>", options.lineOffset ?? 0, options.columnOffset ?? 0);
}

function runInNewContext(code, contextObject, options) {
  options = normalizeOptions(options);
  const context = createContext(contextObject ?? {}, options.contextName ? { name: options.contextName } : undefined);
  return runInContext(code, context, options);
}

function runInThisContext(code, options) {
  options = normalizeOptions(options);
  return ops.vmRun(undefined, `${code}`, options.filename ?? "evalmachine.<anonymous>", options.lineOffset ?? 0, options.columnOffset ?? 0);
}

let compiledFunctions = 0;
function compileFunction(code, params = [], options = {}) {
  if (typeof code !== "string") throw invalidArgType("code", "of type string", code);
  if (!Array.isArray(params)) throw invalidArgType("params", "an instance of Array", params);
  for (const p of params) if (typeof p !== "string") throw invalidArgType("params", "an instance of Array", params);
  options = normalizeOptions(options);
  const extensions = options.contextExtensions ?? [];
  if (!Array.isArray(extensions)) throw invalidArgType("options.contextExtensions", "an instance of Array", extensions);
  const realm = options.parsingContext === undefined ? undefined : realmOf(options.parsingContext);
  // Extensions are scopes around the function (V8 compiles it inside `with` scopes).
  const slot = `__piJsrtCompileFunction${compiledFunctions++}`;
  let src = `(function (${params.join(", ")}) {\n${code}\n})`;
  for (let i = extensions.length - 1; i >= 0; i--) src = `(function () { with (globalThis.${slot}[${i}]) { return ${src}; } })()`;
  const global = realm === undefined ? globalThis : ops.vmGlobal(realm);
  Object.defineProperty(global, slot, { value: extensions, configurable: true, enumerable: false, writable: true });
  try {
    const fn = ops.vmRun(realm, src, options.filename ?? "", options.lineOffset ?? 0, options.columnOffset ?? 0);
    Object.defineProperty(fn, "name", { value: "", configurable: true });
    return fn;
  } finally {
    delete global[slot];
  }
}

function measureMemory() {
  const used = process.memoryUsage().rss;
  return Promise.resolve({ total: { jsMemoryEstimate: used, jsMemoryRange: [used, used] } });
}

const vm = {
  Script,
  createContext,
  isContext,
  runInContext,
  runInNewContext,
  runInThisContext,
  compileFunction,
  measureMemory,
  constants: { USE_MAIN_CONTEXT_DEFAULT_LOADER, DONT_CONTEXTIFY },
};
internal.vmRealmOf = (obj) => contexts.get(obj);
internal.defineBuiltin("vm", () => vm);
