// node:worker_threads — each Worker is another pi-jsrt runtime on its own thread
// (src/ops/worker.rs); messages are structured clones in V8's format through the
// cross-runtime MessagePort registry (js/web/messaging.js).

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;
void globalThis.MessagePort; // loads js/web/messaging.js
const { MessagePort, MessageChannel, BroadcastChannel } = internal.messaging;

const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
const workers = new Map();
const environmentData = new Map();
const untransferable = new WeakSet();
const uncloneable = new WeakSet();

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

const kDefaultLimits = { maxYoungGenerationSizeMb: 48, maxOldGenerationSizeMb: 2048, codeRangeSizeMb: 0, stackSizeMb: 4 };

function validateLimits(limits) {
  if (limits === undefined) return {};
  if (limits === null || typeof limits !== "object") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options.resourceLimits" property must be of type object. Received ${internal.describeReceived(limits)}`);
  }
  const out = {};
  for (const key of Object.keys(kDefaultLimits)) {
    if (limits[key] !== undefined) {
      if (typeof limits[key] !== "number") {
        throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options.resourceLimits.${key}" property must be of type number. Received ${internal.describeReceived(limits[key])}`);
      }
      out[key] = limits[key];
    }
  }
  return out;
}

function decodeDataUrl(url) {
  const m = /^data:([^,]*?)(;base64)?,(.*)$/s.exec(url);
  if (!m) throw codeError(TypeError, "ERR_INVALID_URL", "Invalid URL");
  return m[2] ? Buffer.from(m[3], "base64").toString() : decodeURIComponent(m[3]);
}

class Worker extends EventEmitter {
  #id;
  #port;
  #alive = true;
  #refed = true;
  #limits;
  #exitPromise;
  #resolveExit;

  constructor(filename, options = {}) {
    super();
    if (options === null || typeof options !== "object") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived(options)}`);
    }
    let file = null;
    let code = null;
    if (options.eval) {
      if (typeof filename !== "string") {
        throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'filename' must be a string when options.eval is true. Received ${internal.inspect(filename)}`);
      }
      code = filename;
    } else {
      if (filename instanceof internal.URL || (typeof filename === "object" && filename !== null && typeof filename.href === "string")) {
        const href = String(filename.href);
        if (href.startsWith("data:")) code = decodeDataUrl(href);
        else if (href.startsWith("file:")) file = internal.requireBuiltin("url").fileURLToPath(href);
        else throw codeError(TypeError, "ERR_WORKER_UNSUPPORTED_OPERATION", `The worker script or module filename must be an absolute path or a relative path starting with './' or '../'. Received "${href}"`);
      } else if (typeof filename === "string") {
        if (filename.startsWith("data:")) code = decodeDataUrl(filename);
        else if (filename.startsWith("file:")) file = internal.requireBuiltin("url").fileURLToPath(filename);
        else if (filename.startsWith("/") || filename.startsWith("./") || filename.startsWith("../")) {
          file = internal.requireBuiltin("path").resolve(filename);
        } else {
          throw codeError(
            TypeError,
            "ERR_WORKER_PATH",
            `The worker script or module filename must be an absolute path or a relative path starting with './' or '../'. Received "${filename}"`,
          );
        }
      } else {
        throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "filename" argument must be of type string or an instance of URL. Received ${internal.describeReceived(filename)}`);
      }
    }
    this.#limits = validateLimits(options.resourceLimits);
    let env = options.env;
    if (env === undefined || env === SHARE_ENV) env = process.env;
    else if (env === null || typeof env !== "object") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options.env" property must be of type object or one of undefined, null, or worker.SHARE_ENV. Received ${internal.describeReceived(env)}`);
    }
    const envPairs = [];
    for (const [k, v] of Object.entries(env)) if (v !== undefined) envPairs.push(k, String(v));
    const argv = [process.execPath, file ?? "[worker eval]", ...(Array.isArray(options.argv) ? options.argv.map(String) : [])];
    const data = internal.serializeMessage({ workerData: options.workerData, environmentData: [...environmentData] }, options.transferList);
    const [a, b] = ops.portCreatePair();
    this.#port = internal.createPort(a);
    ops.portDetach(b);
    this.#port.on("message", (message) => this.emit("message", message));
    this.#port.on("messageerror", (error) => this.emit("messageerror", error));
    this.#port.unref();
    this.#exitPromise = new Promise((resolve) => (this.#resolveExit = resolve));
    if (options.stdout) this.stdout = new (internal.requireBuiltin("stream").Readable)({ read() {} });
    else this.stdout = null;
    if (options.stderr) this.stderr = new (internal.requireBuiltin("stream").Readable)({ read() {} });
    else this.stderr = null;
    this.stdin = null;
    const r = ops.workerStart({
      file,
      code,
      argv,
      execArgv: Array.isArray(options.execArgv) ? options.execArgv.map(String) : process.execArgv,
      env: envPairs,
      name: options.name ?? "",
      port: b,
      data: data.bytes,
      dataPorts: data.ports,
      dataBuffers: data.buffers,
      limits: { ...kDefaultLimits, ...this.#limits },
    });
    this.#id = r.id;
    this.threadId = r.threadId;
    workers.set(r.id, this);
    this.performance = {
      eventLoopUtilization: () => ({ idle: 0, active: 0, utilization: 0 }),
    };
  }

  postMessage(message, transfer) {
    this.#port.postMessage(message, transfer);
  }

  terminate(callback) {
    if (typeof callback === "function") {
      process.emitWarning("Passing a callback to worker.terminate() is deprecated. It returns a Promise instead.", "DeprecationWarning", "DEP0132");
      this.#exitPromise.then((code) => callback(null, code));
    }
    if (this.#alive) ops.workerTerminate(this.#id);
    return this.#exitPromise;
  }

  ref() {
    if (this.#alive && !this.#refed) {
      this.#refed = true;
      ops.workerRef(1);
    }
  }

  unref() {
    if (this.#alive && this.#refed) {
      this.#refed = false;
      ops.workerRef(-1);
    }
  }

  get resourceLimits() {
    if (!this.#alive) return {};
    const out = {};
    for (const key of Object.keys(kDefaultLimits)) out[key] = this.#limits[key] ?? (key === "stackSizeMb" ? 4 : -1);
    return out;
  }

  getHeapSnapshot() {
    return Promise.reject(codeError(Error, "ERR_WORKER_NOT_RUNNING", "Worker instance not running"));
  }

  _event(kind, a) {
    switch (kind) {
      case "online":
        this.emit("online");
        break;
      case "stdout":
      case "stderr": {
        const chunk = Buffer.from(a.buffer, a.byteOffset, a.byteLength);
        if (this[kind]) this[kind].push(chunk);
        else process[kind].write(chunk);
        break;
      }
      case "error": {
        let err;
        try {
          err = internal.cloneDeserialize(a, []);
        } catch (e) {
          err = e;
        }
        this.emit("error", err);
        break;
      }
      case "startError":
        this.emit("error", new Error(String(a)));
        break;
      case "exit":
        this.#alive = false;
        if (!this.#refed) ops.workerRef(1); // balance: the exit releases one reference
        workers.delete(this.#id);
        this.stdout?.push(null);
        this.stderr?.push(null);
        this.#port.close();
        this.#resolveExit(a);
        this.emit("exit", a);
        break;
    }
  }
}

internal.workerEvent = (id, kind, a) => workers.get(id)?._event(kind, a);

// ---------------------------------------------------------------------------
// Worker side (called by src/ops/worker.rs on the worker's thread)
// ---------------------------------------------------------------------------

internal.workerMain = async ({ file, code, name, threadId, port, data, dataPorts, dataBuffers, limits }) => {
  internal.isWorker = true;
  const parentPort = internal.createPort(port);
  const init = internal.deserializeMessage(
    new Uint8Array(data),
    dataPorts,
    dataBuffers.map((b) => new Uint8Array(b).buffer),
  ).data;
  for (const [k, v] of init.environmentData ?? []) environmentData.set(k, v);
  internal.workerState = {
    isMainThread: false,
    parentPort,
    workerData: init.workerData,
    threadId,
    resourceLimits: { ...kDefaultLimits, ...(limits ?? {}) },
    name,
  };
  // The parent's 'message' listener lives on the other end; the worker's port keeps the worker
  // alive only while user code listens on it (like Node's parentPort).
  try {
    if (code !== null && code !== undefined) {
      ops.evalScript(code, "[worker eval]");
    } else {
      internal.mainFilename = file;
      await import(file);
    }
  } catch (err) {
    internal.workerFatal(err);
  }
};

internal.workerFatal = (err) => {
  if (internal.workerExiting) return;
  internal.workerExiting = true;
  let bytes;
  try {
    bytes = internal.cloneSerialize(err).bytes;
  } catch {
    bytes = internal.cloneSerialize(new Error(String(err))).bytes;
  }
  ops.workerReport("error", bytes);
  process.exitCode = 1;
  try {
    process.emit("exit", 1);
  } catch {}
  ops.exit(1);
};

internal.workerExit = () => {
  const code = Number(process.exitCode ?? 0) | 0;
  try {
    process.emit("exit", code);
  } catch (err) {
    internal.workerFatal(err);
    return 1;
  }
  return Number(process.exitCode ?? code) | 0;
};

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

function state() {
  return internal.workerState ?? { isMainThread: true, parentPort: null, workerData: null, threadId: 0, resourceLimits: {} };
}

internal.defineBuiltin("worker_threads", () => {
  const s = state();
  return {
    isMainThread: s.isMainThread,
    parentPort: s.parentPort,
    workerData: s.workerData,
    threadId: s.threadId,
    resourceLimits: s.resourceLimits,
    Worker,
    MessageChannel,
    MessagePort,
    BroadcastChannel,
    SHARE_ENV,
    markAsUntransferable(obj) {
      if (obj !== null && (typeof obj === "object" || typeof obj === "function")) untransferable.add(obj);
    },
    isMarkedAsUntransferable: (obj) => obj !== null && typeof obj === "object" && untransferable.has(obj),
    markAsUncloneable(obj) {
      if (obj !== null && (typeof obj === "object" || typeof obj === "function")) uncloneable.add(obj);
    },
    moveMessagePortToContext: (port) => port,
    receiveMessageOnPort: (port) => internal.receiveMessageOnPort(port),
    getEnvironmentData: (key) => environmentData.get(key),
    setEnvironmentData(key, value) {
      if (value === undefined) environmentData.delete(key);
      else environmentData.set(key, value);
    },
  };
});
