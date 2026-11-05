// node:url, readline, assert, module, timers, string_decoder, tty, querystring,
// perf_hooks, async_hooks, worker_threads, console and stubs for unsupported modules.

const EventEmitter = internal.EventEmitter;
const path = internal.requireBuiltin("path");
internal.requireBuiltin("util");

// ---------------------------------------------------------------------------
// url
// ---------------------------------------------------------------------------

function fileURLToPath(url) {
  const u = typeof url === "string" ? new internal.URL(url) : url;
  if (u.protocol !== "file:") {
    const err = new TypeError("The URL must be of scheme file");
    err.code = "ERR_INVALID_URL_SCHEME";
    throw err;
  }
  if (u.hostname !== "" && u.hostname !== "localhost") {
    const err = new TypeError(`File URL host must be "localhost" or empty on ${process.platform}`);
    err.code = "ERR_INVALID_FILE_URL_HOST";
    throw err;
  }
  const p = u.pathname;
  if (/%2f/i.test(p)) {
    const err = new TypeError("File URL path must not include encoded / characters");
    err.code = "ERR_INVALID_FILE_URL_PATH";
    throw err;
  }
  return decodeURIComponent(p);
}

function pathToFileURL(p) {
  let resolved = path.resolve(String(p));
  if (String(p).endsWith("/") && !resolved.endsWith("/")) resolved += "/";
  const encoded = resolved
    .split("/")
    .map((seg) => encodeURIComponent(seg).replace(/%40/g, "@").replace(/%3A/g, ":").replace(/%2B/g, "+").replace(/%2C/g, ",").replace(/%3D/g, "=").replace(/%24/g, "$").replace(/%26/g, "&"))
    .join("/");
  return new internal.URL(`file://${encoded}`);
}

function legacyParse(urlStr, parseQueryString) {
  let u;
  try {
    u = new internal.URL(urlStr);
  } catch {
    const out = { protocol: null, slashes: null, auth: null, host: null, port: null, hostname: null, hash: null, search: null, query: null, pathname: null, path: null, href: urlStr };
    const m = /^([^?#]*)(\?[^#]*)?(#.*)?$/.exec(urlStr) || [];
    out.pathname = m[1] || null;
    out.search = m[2] || null;
    out.query = m[2] ? (parseQueryString ? querystring.parse(m[2].slice(1)) : m[2].slice(1)) : parseQueryString ? {} : null;
    out.hash = m[3] || null;
    out.path = (out.pathname || "") + (out.search || "") || null;
    return out;
  }
  return {
    protocol: u.protocol,
    slashes: true,
    auth: u.username ? `${u.username}${u.password ? `:${u.password}` : ""}` : null,
    host: u.host,
    port: u.port || null,
    hostname: u.hostname,
    hash: u.hash || null,
    search: u.search || null,
    query: parseQueryString ? querystring.parse(u.search.slice(1)) : u.search.slice(1) || null,
    pathname: u.pathname,
    path: u.pathname + u.search,
    href: u.href,
  };
}

function legacyFormat(obj) {
  if (typeof obj === "string") return obj;
  if (obj instanceof internal.URL) return obj.href;
  const protocol = obj.protocol ? (obj.protocol.endsWith(":") ? obj.protocol : obj.protocol + ":") : "";
  const host = obj.host ?? (obj.hostname ? obj.hostname + (obj.port ? `:${obj.port}` : "") : "");
  const auth = obj.auth ? `${obj.auth}@` : "";
  const slashes = obj.slashes || /^(https?|ftp|file|wss?):$/.test(protocol) ? "//" : "";
  let search = obj.search ?? (obj.query && typeof obj.query === "object" ? "?" + querystring.stringify(obj.query) : "");
  if (search === "?") search = "";
  const hash = obj.hash ?? "";
  return `${protocol}${host || auth ? slashes : ""}${auth}${host}${obj.pathname ?? ""}${search}${hash}`;
}

const url = {
  URL: internal.URL,
  URLSearchParams: internal.URLSearchParams,
  fileURLToPath,
  pathToFileURL,
  parse: legacyParse,
  format: legacyFormat,
  resolve: (from, to) => {
    const r = new internal.URL(to, new internal.URL(from, "resolve://"));
    if (r.protocol === "resolve:") {
      const { pathname, search, hash } = r;
      return pathname + search + hash;
    }
    return r.toString();
  },
  domainToASCII: (d) => ops.domainToASCII(String(d)),
  domainToUnicode: (d) => String(d),
  urlToHttpOptions: (u) => ({ protocol: u.protocol, hostname: u.hostname, hash: u.hash, search: u.search, pathname: u.pathname, path: u.pathname + u.search, href: u.href, port: u.port ? Number(u.port) : undefined }),
};
internal.defineBuiltin("url", () => url);

// ---------------------------------------------------------------------------
// querystring
// ---------------------------------------------------------------------------

const querystring = {
  escape: (s) => encodeURIComponent(s),
  unescape: (s) => {
    try {
      return decodeURIComponent(s);
    } catch {
      return s;
    }
  },
  parse(str, sep = "&", eq = "=") {
    const out = Object.create(null);
    if (typeof str !== "string" || str.length === 0) return out;
    for (const part of str.split(sep)) {
      if (!part) continue;
      const i = part.indexOf(eq);
      const k = querystring.unescape((i >= 0 ? part.slice(0, i) : part).replace(/\+/g, " "));
      const v = querystring.unescape((i >= 0 ? part.slice(i + eq.length) : "").replace(/\+/g, " "));
      if (k in out) out[k] = Array.isArray(out[k]) ? [...out[k], v] : [out[k], v];
      else out[k] = v;
    }
    return out;
  },
  stringify(obj, sep = "&", eq = "=") {
    if (!obj || typeof obj !== "object") return "";
    const enc = (v) => encodeURIComponent(typeof v === "string" || typeof v === "number" || typeof v === "boolean" || typeof v === "bigint" ? String(v) : "");
    return Object.keys(obj)
      .map((k) => (Array.isArray(obj[k]) ? obj[k].map((v) => `${enc(k)}${eq}${enc(v)}`).join(sep) : `${enc(k)}${eq}${enc(obj[k])}`))
      .filter(Boolean)
      .join(sep);
  },
};
querystring.decode = querystring.parse;
querystring.encode = querystring.stringify;
internal.defineBuiltin("querystring", () => querystring);

// ---------------------------------------------------------------------------
// string_decoder
// ---------------------------------------------------------------------------

class StringDecoder {
  constructor(encoding = "utf8") {
    this.encoding = String(encoding).toLowerCase().replace("-", "");
    if (this.encoding === "utf8") this._d = new internal.TextDecoder("utf-8");
    else if (this.encoding === "utf16le" || this.encoding === "ucs2") this._d = new internal.TextDecoder("utf-16le");
    else this._d = null;
  }
  write(buf) {
    if (typeof buf === "string") return buf;
    if (this._d) return this._d.decode(buf, { stream: true });
    return internal.Buffer.from(buf).toString(this.encoding);
  }
  end(buf) {
    let out = buf ? this.write(buf) : "";
    if (this._d) out += this._d.decode();
    return out;
  }
}
internal.defineBuiltin("string_decoder", () => ({ StringDecoder }));

// ---------------------------------------------------------------------------
// readline
// ---------------------------------------------------------------------------

class Interface extends EventEmitter {
  constructor(input, output, completer, terminal) {
    super();
    let opts = input;
    if (!(input && typeof input === "object" && ("input" in input || "output" in input) && !(input instanceof EventEmitter))) {
      opts = { input, output, completer, terminal };
    }
    this.input = opts.input;
    this.output = opts.output;
    this.terminal = Boolean(opts.terminal ?? opts.output?.isTTY);
    this.line = "";
    this.closed = false;
    this._prompt = opts.prompt ?? "> ";
    this._questions = [];
    this._buffer = "";
    this._paused = false;
    this._lineQueue = [];
    this._decoder = new internal.TextDecoder();
    const crlfDelay = opts.crlfDelay;
    void crlfDelay;
    this._ondata = (chunk) => {
      this._buffer += typeof chunk === "string" ? chunk : this._decoder.decode(chunk, { stream: true });
      let idx;
      while ((idx = this._buffer.search(/\r?\n|\r/)) >= 0) {
        const m = /\r?\n|\r/.exec(this._buffer.slice(idx));
        const line = this._buffer.slice(0, idx);
        this._buffer = this._buffer.slice(idx + m[0].length);
        this._emitLine(line);
      }
    };
    this._onend = () => {
      if (this._buffer.length) {
        const rest = this._buffer;
        this._buffer = "";
        this._emitLine(rest);
      }
      this.close();
    };
    if (this.input && typeof this.input.on === "function") {
      this.input.on("data", this._ondata);
      this.input.on("end", this._onend);
      if (typeof this.input.resume === "function") this.input.resume();
    }
  }
  _emitLine(line) {
    const q = this._questions.shift();
    if (q) q(line);
    else this.emit("line", line);
  }
  setPrompt(p) {
    this._prompt = p;
  }
  getPrompt() {
    return this._prompt;
  }
  prompt() {
    this.output?.write?.(this._prompt);
  }
  question(query, options, cb) {
    if (typeof options === "function") cb = options;
    if (this.closed) {
      const err = new Error("readline was closed");
      err.code = "ERR_USE_AFTER_CLOSE";
      throw err;
    }
    this.output?.write?.(query);
    this._questions.push(cb);
  }
  write(data) {
    if (typeof data === "string") this._ondata(data);
  }
  pause() {
    this._paused = true;
    this.input?.pause?.();
    this.emit("pause");
    return this;
  }
  resume() {
    this._paused = false;
    this.input?.resume?.();
    this.emit("resume");
    return this;
  }
  close() {
    if (this.closed) return;
    this.closed = true;
    if (this.input && typeof this.input.off === "function") {
      this.input.off("data", this._ondata);
      this.input.off("end", this._onend);
    }
    this.emit("close");
  }
  getCursorPos() {
    return { rows: 0, cols: this.line.length };
  }
  [Symbol.asyncIterator]() {
    const queue = [];
    const waiters = [];
    let done = false;
    const onLine = (l) => {
      const w = waiters.shift();
      if (w) w({ value: l, done: false });
      else queue.push(l);
    };
    const onClose = () => {
      done = true;
      for (const w of waiters.splice(0)) w({ value: undefined, done: true });
    };
    this.on("line", onLine);
    this.once("close", onClose);
    return {
      next: () => {
        if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
        if (done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((r) => waiters.push(r));
      },
      return: () => {
        this.off("line", onLine);
        this.close();
        return Promise.resolve({ value: undefined, done: true });
      },
      [Symbol.asyncIterator]() {
        return this;
      },
    };
  }
}

const cursorTo = (stream, x, y, cb) => {
  stream?.write?.(typeof y === "number" ? `\x1b[${y + 1};${x + 1}H` : `\x1b[${x + 1}G`);
  if (typeof y === "function") y();
  else if (typeof cb === "function") cb();
  return true;
};
const moveCursor = (stream, dx, dy, cb) => {
  let s = "";
  if (dx < 0) s += `\x1b[${-dx}D`;
  else if (dx > 0) s += `\x1b[${dx}C`;
  if (dy < 0) s += `\x1b[${-dy}A`;
  else if (dy > 0) s += `\x1b[${dy}B`;
  stream?.write?.(s);
  if (typeof cb === "function") cb();
  return true;
};
const clearLine = (stream, dir, cb) => {
  stream?.write?.(dir < 0 ? "\x1b[1K" : dir > 0 ? "\x1b[0K" : "\x1b[2K");
  if (typeof cb === "function") cb();
  return true;
};
const clearScreenDown = (stream, cb) => {
  stream?.write?.("\x1b[0J");
  if (typeof cb === "function") cb();
  return true;
};

const readline = {
  Interface,
  createInterface: (input, output, completer, terminal) => new Interface(input, output, completer, terminal),
  cursorTo,
  moveCursor,
  clearLine,
  clearScreenDown,
  emitKeypressEvents: () => {},
};
const readlinePromises = {
  Interface,
  createInterface: (...args) => {
    const rl = new Interface(...args);
    const q = rl.question.bind(rl);
    rl.question = (query, options) =>
      new Promise((resolve, reject) => {
        if (options?.signal?.aborted) return reject(options.signal.reason);
        q(query, resolve);
      });
    return rl;
  },
  Readline: class Readline {
    constructor(stream) {
      this.stream = stream;
      this.buf = "";
    }
    cursorTo(x, y) {
      this.buf += typeof y === "number" ? `\x1b[${y + 1};${x + 1}H` : `\x1b[${x + 1}G`;
      return this;
    }
    clearLine(dir) {
      this.buf += dir < 0 ? "\x1b[1K" : dir > 0 ? "\x1b[0K" : "\x1b[2K";
      return this;
    }
    async commit() {
      this.stream.write(this.buf);
      this.buf = "";
    }
  },
};
readline.promises = readlinePromises;
internal.defineBuiltin("readline", () => readline);
internal.defineBuiltin("readline/promises", () => readlinePromises);

// ---------------------------------------------------------------------------
// assert
// ---------------------------------------------------------------------------

class AssertionError extends Error {
  constructor(options) {
    super(options.message ?? `${internal.inspect(options.actual)} ${options.operator} ${internal.inspect(options.expected)}`);
    this.name = "AssertionError";
    this.code = "ERR_ASSERTION";
    this.actual = options.actual;
    this.expected = options.expected;
    this.operator = options.operator;
    this.generatedMessage = !options.message;
  }
}

function fail(actual, expected, message, operator) {
  if (message instanceof Error) throw message;
  throw new AssertionError({ actual, expected, message, operator });
}

function matchesError(err, expected) {
  if (expected === undefined) return true;
  if (typeof expected === "function") {
    if (expected.prototype !== undefined && err instanceof expected) return true;
    if (Error.isPrototypeOf(expected) || expected === Error) return false;
    return expected(err) === true;
  }
  if (expected instanceof RegExp) return expected.test(String(err?.message ?? err));
  if (typeof expected === "object") {
    return Object.keys(expected).every((k) =>
      expected[k] instanceof RegExp && typeof err[k] === "string" ? expected[k].test(err[k]) : internal.isDeepEqual(err[k], expected[k]),
    );
  }
  return true;
}

function makeAssert(strict) {
  const assert = (value, message) => {
    if (!value) fail(value, true, message ?? "The expression evaluated to a falsy value", "==");
  };
  Object.assign(assert, {
    AssertionError,
    ok: (value, message) => {
      if (!value) fail(value, true, message ?? "The expression evaluated to a falsy value", "==");
    },
    fail: (message = "Failed") => fail(undefined, undefined, message, "fail"),
    equal: strict
      ? (a, b, m) => Object.is(a, b) || fail(a, b, m, "strictEqual")
      : (a, b, m) => a == b || (Number.isNaN(a) && Number.isNaN(b)) || fail(a, b, m, "=="),
    notEqual: strict ? (a, b, m) => !Object.is(a, b) || fail(a, b, m, "notStrictEqual") : (a, b, m) => a != b || fail(a, b, m, "!="),
    strictEqual: (a, b, m) => Object.is(a, b) || fail(a, b, m, "strictEqual"),
    notStrictEqual: (a, b, m) => !Object.is(a, b) || fail(a, b, m, "notStrictEqual"),
    deepEqual: (a, b, m) => internal.isDeepEqual(a, b, strict) || fail(a, b, m, "deepEqual"),
    notDeepEqual: (a, b, m) => !internal.isDeepEqual(a, b, strict) || fail(a, b, m, "notDeepEqual"),
    deepStrictEqual: (a, b, m) => internal.isDeepEqual(a, b, true) || fail(a, b, m, "deepStrictEqual"),
    notDeepStrictEqual: (a, b, m) => !internal.isDeepEqual(a, b, true) || fail(a, b, m, "notDeepStrictEqual"),
    throws(fn, expected, message) {
      if (typeof expected === "string") {
        message = expected;
        expected = undefined;
      }
      try {
        fn();
      } catch (err) {
        if (!matchesError(err, expected)) throw err;
        return;
      }
      fail(undefined, expected, message ?? "Missing expected exception.", "throws");
    },
    doesNotThrow(fn, message) {
      try {
        fn();
      } catch (err) {
        fail(err, undefined, typeof message === "string" ? message : `Got unwanted exception.\nActual message: "${err?.message}"`, "doesNotThrow");
      }
    },
    async rejects(promiseOrFn, expected, message) {
      if (typeof expected === "string") {
        message = expected;
        expected = undefined;
      }
      try {
        await (typeof promiseOrFn === "function" ? promiseOrFn() : promiseOrFn);
      } catch (err) {
        if (!matchesError(err, expected)) throw err;
        return;
      }
      fail(undefined, expected, message ?? "Missing expected rejection.", "rejects");
    },
    async doesNotReject(promiseOrFn, message) {
      try {
        await (typeof promiseOrFn === "function" ? promiseOrFn() : promiseOrFn);
      } catch (err) {
        fail(err, undefined, message ?? `Got unwanted rejection.\nActual message: "${err?.message}"`, "doesNotReject");
      }
    },
    match: (s, re, m) => re.test(s) || fail(s, re, m ?? `The input did not match the regular expression ${re}. Input:\n\n${internal.inspect(s)}\n`, "match"),
    doesNotMatch: (s, re, m) => !re.test(s) || fail(s, re, m, "doesNotMatch"),
    ifError: (v) => {
      if (v !== null && v !== undefined) throw v;
    },
  });
  return assert;
}
const assert = makeAssert(false);
assert.strict = makeAssert(true);
assert.strict.strict = assert.strict;
internal.defineBuiltin("assert", () => assert);
internal.defineBuiltin("assert/strict", () => assert.strict);

// ---------------------------------------------------------------------------
// module
// ---------------------------------------------------------------------------

function createRequire(filename) {
  let f = filename;
  if (f instanceof internal.URL) f = fileURLToPath(f);
  else if (typeof f === "string" && f.startsWith("file:")) f = fileURLToPath(f);
  if (typeof f !== "string") {
    const err = new TypeError("The argument 'filename' must be a file URL object, file URL string, or absolute path string.");
    err.code = "ERR_INVALID_ARG_VALUE";
    throw err;
  }
  if (f.endsWith("/")) f += "noop.js";
  return internal.makeRequire(f);
}

const builtinModules = () =>
  internal
    .builtinNames()
    .filter((n) => !n.startsWith("jsrt:"))
    .sort();

class Module {
  constructor(id = "", parent) {
    this.id = id;
    this.exports = {};
    this.parent = parent;
    this.filename = null;
    this.loaded = false;
    this.children = [];
    this.paths = [];
  }
}
Module.createRequire = createRequire;
Object.defineProperty(Module, "builtinModules", { get: builtinModules, enumerable: true });
Module.isBuiltin = (name) => internal.hasBuiltin(String(name).replace(/^node:/, ""));
Module.register = () => {};
Module.syncBuiltinESMExports = () => {};
Module.findSourceMap = () => undefined;
Module.enableCompileCache = () => ({ status: 0 });
Module.stripTypeScriptTypes = (code) => ops.transpile(String(code), "input.ts");
Module.Module = Module;
internal.defineBuiltin("module", () => Module);

// ---------------------------------------------------------------------------
// timers, timers/promises
// ---------------------------------------------------------------------------

const abortError = (signal) => {
  const err = new internal.DOMException("The operation was aborted", "AbortError");
  err.code = "ABORT_ERR";
  err.cause = signal?.reason;
  return err;
};

const timersPromises = {
  setTimeout(ms, value, options = {}) {
    return new Promise((resolve, reject) => {
      const signal = options?.signal;
      if (signal?.aborted) return reject(abortError(signal));
      const t = setTimeout(() => {
        signal?.removeEventListener("abort", onAbort);
        resolve(value);
      }, ms);
      const onAbort = () => {
        clearTimeout(t);
        reject(abortError(signal));
      };
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  },
  setImmediate(value, options = {}) {
    return new Promise((resolve, reject) => {
      const signal = options?.signal;
      if (signal?.aborted) return reject(abortError(signal));
      const t = setImmediate(() => resolve(value));
      signal?.addEventListener(
        "abort",
        () => {
          clearImmediate(t);
          reject(abortError(signal));
        },
        { once: true },
      );
    });
  },
  async *setInterval(ms, value, options = {}) {
    const signal = options?.signal;
    let pending = 0;
    let wake = null;
    const t = setInterval(() => {
      pending++;
      wake?.();
    }, ms);
    try {
      for (;;) {
        if (signal?.aborted) throw abortError(signal);
        if (pending === 0) {
          await new Promise((r) => {
            wake = r;
            signal?.addEventListener("abort", r, { once: true });
          });
          wake = null;
          continue;
        }
        pending--;
        yield value;
      }
    } finally {
      clearInterval(t);
    }
  },
  scheduler: {
    wait: (ms, options) => timersPromises.setTimeout(ms, undefined, options),
    yield: () => timersPromises.setImmediate(),
  },
};
internal.defineBuiltin("timers", () => ({ ...internal.timers, promises: timersPromises }));
internal.defineBuiltin("timers/promises", () => timersPromises);

// ---------------------------------------------------------------------------
// tty, perf_hooks, async_hooks, worker_threads, console
// ---------------------------------------------------------------------------

internal.defineBuiltin("tty", () => ({
  isatty: (fd) => ops.isatty(Number(fd)),
  ReadStream: class ReadStream extends EventEmitter {},
  WriteStream: class WriteStream extends EventEmitter {},
}));



// AsyncLocalStorage and async_hooks over the engine's async context (src/engine.rs): the
// frame (a Map of AsyncLocalStorage -> store) and async ids are captured when promise
// reactions, timers and AsyncResources are created and restored while their callbacks run.

const EMPTY_FRAME = new Map();
const currentFrame = () => ops.asyncContext()[0] ?? EMPTY_FRAME;
const activeHooks = new Set();
const hookResources = new Map();

function emitHook(kind, id, type, trigger) {
  for (const hook of [...activeHooks]) {
    const fn = hook[kind];
    if (typeof fn !== "function") continue;
    if (kind === "init") {
      let resource = hookResources.get(id);
      if (!resource) {
        resource = {};
        hookResources.set(id, resource);
      }
      fn.call(hook, id, type, trigger, resource);
    } else {
      fn.call(hook, id);
    }
  }
  if (kind === "destroy") hookResources.delete(id);
}

class AsyncHook {
  constructor({ init, before, after, destroy, promiseResolve } = {}) {
    for (const [name, fn] of Object.entries({ init, before, after, destroy, promiseResolve })) {
      if (fn !== undefined && typeof fn !== "function") {
        const err = new TypeError(`hook.${name} must be a function`);
        err.code = "ERR_ASYNC_CALLBACK";
        throw err;
      }
    }
    this.init = init;
    this.before = before;
    this.after = after;
    this.destroy = destroy;
    this.promiseResolve = promiseResolve;
  }
  enable() {
    activeHooks.add(this);
    ops.asyncHooksEnable(emitHook);
    return this;
  }
  disable() {
    activeHooks.delete(this);
    if (activeHooks.size === 0) ops.asyncHooksEnable(null);
    return this;
  }
}

class AsyncResource {
  #ctx;
  #destroyed = false;
  constructor(type, opts = {}) {
    if (typeof type !== "string") {
      const err = new TypeError(`The "type" argument must be of type string. Received ${internal.describeReceived(type)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    let triggerAsyncId = opts;
    if (typeof opts !== "number") triggerAsyncId = opts?.triggerAsyncId;
    if (triggerAsyncId !== undefined && (!Number.isSafeInteger(triggerAsyncId) || triggerAsyncId < -1)) {
      const err = new RangeError(`Invalid triggerAsyncId value: ${triggerAsyncId}`);
      err.code = "ERR_INVALID_ASYNC_ID";
      throw err;
    }
    this.#ctx = ops.asyncResourceNew(type, triggerAsyncId);
  }
  runInAsyncScope(fn, thisArg, ...args) {
    const previous = ops.asyncEnter(this.#ctx);
    const id = this.asyncId();
    if (activeHooks.size) emitHook("before", id);
    try {
      return Reflect.apply(fn, thisArg, args);
    } finally {
      if (activeHooks.size) emitHook("after", id);
      ops.asyncEnter(previous);
    }
  }
  emitDestroy() {
    if (!this.#destroyed) {
      this.#destroyed = true;
      if (activeHooks.size) emitHook("destroy", this.asyncId());
    }
    return this;
  }
  asyncId() {
    return ops.asyncResourceInfo(this.#ctx)[0];
  }
  triggerAsyncId() {
    return ops.asyncResourceInfo(this.#ctx)[1];
  }
  bind(fn, thisArg) {
    if (typeof fn !== "function") {
      const err = new TypeError(`The "fn" argument must be of type function. Received ${internal.describeReceived(fn)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    const resource = this;
    const bound =
      thisArg === undefined
        ? function (...args) {
            return resource.runInAsyncScope(fn, this, ...args);
          }
        : (...args) => resource.runInAsyncScope(fn, thisArg, ...args);
    Object.defineProperties(bound, {
      length: { configurable: true, enumerable: false, value: fn.length, writable: false },
      asyncResource: { configurable: true, enumerable: true, value: this, writable: true },
    });
    return bound;
  }
  static bind(fn, type, thisArg) {
    type = type || fn.name || "bound-anonymous-fn";
    return new AsyncResource(type).bind(fn, thisArg);
  }
}

class AsyncLocalStorage {
  #enabled = false;
  constructor(options = {}) {
    if (options === null || typeof options !== "object") {
      const err = new TypeError(`The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
  }
  static bind(fn) {
    return AsyncResource.bind(fn);
  }
  static snapshot() {
    return AsyncResource.bind(function runInAsyncScope(cb, ...args) {
      return Reflect.apply(cb, this, args);
    });
  }
  disable() {
    this.#enabled = false;
  }
  _enable() {
    this.#enabled = true;
  }
  getStore() {
    if (!this.#enabled) return undefined;
    return currentFrame().get(this);
  }
  enterWith(store) {
    this.#enabled = true;
    const next = new Map(currentFrame());
    next.set(this, store);
    ops.asyncFrameSet(next);
  }
  run(store, callback, ...args) {
    const previous = currentFrame();
    if (this.#enabled && previous.has(this) && Object.is(previous.get(this), store)) return Reflect.apply(callback, null, args);
    this.#enabled = true;
    const next = new Map(previous);
    next.set(this, store);
    ops.asyncFrameSet(next);
    try {
      return Reflect.apply(callback, null, args);
    } finally {
      ops.asyncFrameSet(previous === EMPTY_FRAME ? undefined : previous);
    }
  }
  exit(callback, ...args) {
    if (!this.#enabled) return Reflect.apply(callback, null, args);
    this.disable();
    try {
      return Reflect.apply(callback, null, args);
    } finally {
      this._enable();
    }
  }
}

internal.AsyncResource = AsyncResource;
internal.defineBuiltin("async_hooks", () => ({
  AsyncLocalStorage,
  AsyncResource,
  AsyncHook,
  createHook: (callbacks) => new AsyncHook(callbacks),
  executionAsyncId: () => ops.asyncContext()[1],
  triggerAsyncId: () => ops.asyncContext()[2],
  executionAsyncResource: () => {
    const id = ops.asyncContext()[1];
    let resource = hookResources.get(id);
    if (!resource) {
      resource = {};
      hookResources.set(id, resource);
    }
    return resource;
  },
  asyncWrapProviders: Object.freeze(Object.assign(Object.create(null), {"NONE":0,"DIRHANDLE":1,"DNSCHANNEL":2,"ELDHISTOGRAM":3,"FILEHANDLE":4,"FILEHANDLECLOSEREQ":5,"BLOBREADER":6,"FSEVENTWRAP":7,"FSREQCALLBACK":8,"FSREQPROMISE":9,"GETADDRINFOREQWRAP":10,"GETNAMEINFOREQWRAP":11,"HEAPSNAPSHOT":12,"HTTP2SESSION":13,"HTTP2STREAM":14,"HTTP2PING":15,"HTTP2SETTINGS":16,"HTTPINCOMINGMESSAGE":17,"HTTPCLIENTREQUEST":18,"JSSTREAM":19,"JSUDPWRAP":20,"MESSAGEPORT":21,"PIPECONNECTWRAP":22,"PIPESERVERWRAP":23,"PIPEWRAP":24,"PROCESSWRAP":25,"PROMISE":26,"QUERYWRAP":27,"QUIC_ENDPOINT":28,"QUIC_LOGSTREAM":29,"QUIC_PACKET":30,"QUIC_SESSION":31,"QUIC_STREAM":32,"QUIC_UDP":33,"SHUTDOWNWRAP":34,"SIGNALWRAP":35,"STATWATCHER":36,"STREAMPIPE":37,"TCPCONNECTWRAP":38,"TCPSERVERWRAP":39,"TCPWRAP":40,"TTYWRAP":41,"UDPSENDWRAP":42,"UDPWRAP":43,"SIGINTWATCHDOG":44,"WORKER":45,"WORKERCPUPROFILE":46,"WORKERCPUUSAGE":47,"WORKERHEAPSNAPSHOT":48,"WORKERHEAPSTATISTICS":49,"WRITEWRAP":50,"ZLIB":51,"CHECKPRIMEREQUEST":52,"PBKDF2REQUEST":53,"KEYPAIRGENREQUEST":54,"KEYGENREQUEST":55,"KEYEXPORTREQUEST":56,"CIPHERREQUEST":57,"DERIVEBITSREQUEST":58,"HASHREQUEST":59,"RANDOMBYTESREQUEST":60,"RANDOMPRIMEREQUEST":61,"SCRYPTREQUEST":62,"SIGNREQUEST":63,"TLSWRAP":64,"VERIFYREQUEST":65})),
}));

internal.defineBuiltin("console", () => globalThis.console);

// Stubs: importable, but calling them throws.
function stubModule(name, names, extra = {}) {
  internal.defineBuiltin(name, () => {
    const mod = { ...extra };
    for (const n of names) {
      if (!(n in mod)) {
        mod[n] = function () {
          throw new Error(`${name}.${n} is not supported in pi-jsrt`);
        };
      }
    }
    return mod;
  });
}
stubModule("inspector", ["open", "close", "url", "Session"]);
stubModule("constants", [], {});
