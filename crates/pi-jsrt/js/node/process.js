// global `process`, `global`

const EventEmitter = internal.EventEmitter;
const config = ops.config;

function makeWriteStream(fd) {
  const s = new EventEmitter();
  s.fd = fd;
  s.writable = true;
  s._isStdio = true;
  const tty = ops.isatty(fd);
  s.isTTY = tty || undefined;
  const size = () => (tty ? ops.termSize(fd) : null);
  Object.defineProperty(s, "columns", { get: () => size()?.[0] ?? (tty ? 80 : undefined), enumerable: true, configurable: true });
  Object.defineProperty(s, "rows", { get: () => size()?.[1] ?? (tty ? 24 : undefined), enumerable: true, configurable: true });
  s.getWindowSize = () => [s.columns ?? 80, s.rows ?? 24];
  s.hasColors = (count = 16) => tty && count <= 256;
  s.getColorDepth = () => (tty ? 8 : 1);
  s.write = (chunk, encoding, cb) => {
    if (typeof encoding === "function") {
      cb = encoding;
      encoding = undefined;
    }
    if (typeof chunk === "string" && encoding && encoding !== "utf8" && encoding !== "utf-8") {
      chunk = internal.Buffer.from(chunk, encoding);
    }
    ops.write(fd, chunk);
    if (typeof cb === "function") queueMicrotask(() => cb(null));
    return true;
  };
  s.end = (chunk, encoding, cb) => {
    if (typeof chunk === "function") {
      cb = chunk;
      chunk = undefined;
    }
    if (chunk !== undefined && chunk !== null) s.write(chunk, encoding);
    if (typeof cb === "function") queueMicrotask(cb);
    return s;
  };
  s.cork = () => {};
  s.uncork = () => {};
  s.setDefaultEncoding = () => s;
  s.cursorTo = (x, y, cb) => {
    s.write(typeof y === "number" ? `\x1b[${y + 1};${x + 1}H` : `\x1b[${x + 1}G`);
    if (typeof cb === "function") cb();
    return true;
  };
  s.moveCursor = (dx, dy, cb) => {
    let out = "";
    if (dx < 0) out += `\x1b[${-dx}D`;
    else if (dx > 0) out += `\x1b[${dx}C`;
    if (dy < 0) out += `\x1b[${-dy}A`;
    else if (dy > 0) out += `\x1b[${dy}B`;
    s.write(out);
    if (typeof cb === "function") cb();
    return true;
  };
  s.clearLine = (dir, cb) => {
    s.write(dir < 0 ? "\x1b[1K" : dir > 0 ? "\x1b[0K" : "\x1b[2K");
    if (typeof cb === "function") cb();
    return true;
  };
  s.clearScreenDown = (cb) => {
    s.write("\x1b[0J");
    if (typeof cb === "function") cb();
    return true;
  };
  return s;
}

function makeStdin() {
  const s = new EventEmitter();
  s.fd = 0;
  s.isTTY = ops.isatty(0) || undefined;
  s.readable = true;
  s.setRawMode = () => s;
  s.setEncoding = () => s;
  s.resume = () => s;
  s.pause = () => s;
  s.ref = () => s;
  s.unref = () => s;
  s.read = () => null;
  s[Symbol.asyncIterator] = async function* () {};
  return s;
}

// process.stdin when the runtime reads the real standard input (Node mode): a Readable over
// one-chunk reads of fd 0 (ops.stdinRead); it keeps the loop alive while flowing.
function makeReadableStdin() {
  const { Readable } = internal.requireBuiltin("stream");
  const tty = ops.isatty(0);
  let outstanding = false;
  const loop = internal.loopHandle();
  const s = new Readable({
    highWaterMark: 65536,
    read() {
      if (outstanding) return;
      outstanding = true;
      loop.activate();
      if (!ops.stdinRead()) {
        outstanding = false;
        loop.deactivate();
        this.push(null);
      }
    },
  });
  const updateRef = () => {
    if (outstanding && !s.isPaused()) loop.activate();
    else loop.deactivate();
  };
  internal.stdinEvent = (kind, data) => {
    outstanding = false;
    loop.deactivate();
    switch (kind) {
      case "data":
        s.push(internal.Buffer.from(data.buffer, data.byteOffset, data.byteLength));
        break;
      case "end":
        s.push(null);
        break;
      case "error":
        s.destroy(data);
        break;
      case "retry":
        if (!s.isPaused() || s.readableLength < s.readableHighWaterMark) s._read();
        break;
    }
  };
  const pause = s.pause;
  s.pause = function () {
    const r = pause.call(this);
    updateRef();
    return r;
  };
  const resume = s.resume;
  s.resume = function () {
    const r = resume.call(this);
    updateRef();
    return r;
  };
  s.fd = 0;
  s.isTTY = tty || undefined;
  s.isRaw = false;
  s.setRawMode = function setRawMode(flag) {
    flag = Boolean(flag);
    const err = ops.setRawMode(0, flag);
    if (err) {
      const e = new Error(`setRawMode ${err === -25 ? "ENOTTY" : "EIO"}`);
      e.errno = err;
      e.code = err === -25 ? "ENOTTY" : "EIO";
      e.syscall = "setRawMode";
      throw e;
    }
    this.isRaw = flag;
    return this;
  };
  s.ref = () => {
    loop.ref();
    return s;
  };
  s.unref = () => {
    loop.unref();
    return s;
  };
  s.setTimeout = () => s;
  return s;
}

// process.nextTick queue, drained by the engine before promise jobs (internal.runTicks).
let tickQueue = [];
let tickHead = 0;
internal.runTicks = () => {
  while (tickHead < tickQueue.length) {
    const [callback, args, asyncCtx] = tickQueue[tickHead];
    tickQueue[tickHead++] = undefined;
    const previous = ops.asyncEnter(asyncCtx);
    try {
      Reflect.apply(callback, undefined, args);
    } catch (err) {
      ops.reportError("Uncaught exception", err);
    } finally {
      ops.asyncEnter(previous);
    }
  }
  tickQueue = [];
  tickHead = 0;
};

/**
 * `process.env` as Node keeps it: values are strings, and every assignment or deletion reaches the
 * runtime (`ops.setEnv`), so `process.env.TZ = "Asia/Kolkata"` changes the runtime's timezone.
 */
function liveEnv(values) {
  return new Proxy(values, {
    set(target, key, value) {
      if (typeof key === "symbol") throw new TypeError("Cannot convert a Symbol value to a string");
      const text = String(value);
      target[key] = text;
      ops.setEnv(key, text);
      return true;
    },
    defineProperty(target, key, descriptor) {
      if (typeof key === "symbol" || !("value" in descriptor)) {
        throw new TypeError("'process.env' only accepts a configurable, writable, and enumerable data descriptor");
      }
      const text = String(descriptor.value);
      Object.defineProperty(target, key, { value: text, writable: true, enumerable: true, configurable: true });
      ops.setEnv(key, text);
      return true;
    },
    deleteProperty(target, key) {
      delete target[key];
      if (typeof key === "string") ops.setEnv(key, undefined);
      return true;
    },
  });
}

const process = new EventEmitter();
const startTime = ops.now();
let exitCode;

Object.assign(process, {
  title: "pi",
  // pi's `engines` minimum (">=22.19.0"), the version pi-ai reports in User-Agent/Stainless
  // headers too; it satisfies the version checks of the packages in tests/npm_packages.rs.
  // No `napi`/`openssl` entries: native addons cannot load, and the crypto is not OpenSSL.
  version: "v22.19.0",
  versions: {
    node: "22.19.0", pi_jsrt: config.version, quickjs: "0.16.2", v8: "0.0", uv: "1.51.0", modules: "127",
    zlib: "1.3.1", brotli: "1.1.0", unicode: "16.0",
  },
  release: { name: "node", lts: "Jod" },
  platform: config.platform,
  arch: config.arch,
  pid: config.pid,
  ppid: config.ppid,
  execPath: config.execPath,
  execArgv: [],
  argv: Array.from(config.argv),
  argv0: config.argv[0] ?? "pi",
  env: liveEnv(config.env),
  features: { inspector: false, ipv6: true, tls: true, typescript: "strip" },
  config: { variables: {} },
  allowedNodeEnvironmentFlags: new Set(),
  stdout: makeWriteStream(1),
  stderr: makeWriteStream(2),
  cwd: () => ops.cwd(),
  chdir: (dir) => ops.chdir(String(dir)),
  uptime: () => (ops.now() - startTime) / 1000,
  hrtime: Object.assign(
    function hrtime(prev) {
      const ns = ops.hrtime();
      let sec = Math.floor(ns / 1e9);
      let nsec = Math.floor(ns % 1e9);
      if (prev) {
        sec -= prev[0];
        nsec -= prev[1];
        if (nsec < 0) {
          sec -= 1;
          nsec += 1e9;
        }
      }
      return [sec, nsec];
    },
    { bigint: () => BigInt(Math.floor(ops.hrtime())) },
  ),
  memoryUsage: Object.assign(() => ops.memoryUsage(), { rss: () => ops.memoryUsage().rss }),
  cpuUsage: (prev) => {
    const u = ops.cpuUsage();
    return prev ? { user: u.user - prev.user, system: u.system - prev.system } : u;
  },
  resourceUsage: () => ({ ...ops.cpuUsage(), maxRSS: ops.memoryUsage().rss / 1024 }),
  nextTick(callback, ...args) {
    if (typeof callback !== "function") {
      const err = new TypeError(`The "callback" argument must be of type function. Received ${internal.describeReceived(callback)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    if (process._exiting) return;
    if (tickQueue.length === tickHead) ops.ticksScheduled();
    tickQueue.push([callback, args, ops.asyncResourceNew("TickObject")]);
  },
  emitWarning(warning, type, code) {
    if (typeof type === "object" && type) {
      code = type.code;
      type = type.type;
    }
    const w = warning instanceof Error ? warning : Object.assign(new Error(String(warning)), { name: type || "Warning" });
    if (code) w.code = code;
    if (w.name === "DeprecationWarning" && process.noDeprecation) return;
    if (w.name === "DeprecationWarning" && process.throwDeprecation) throw w;
    if (nodeMode) {
      // Node: emitted on the next tick; the default handler prints every warning.
      process.nextTick(() => {
        process.emit("warning", w);
        if (process.execArgv.includes("--no-warnings")) return;
        let msg = `(${process.release.name}:${process.pid}) ${w.code ? `[${w.code}] ` : ""}${w.name}: ${w.message}`;
        if (!warnedOnce) {
          warnedOnce = true;
          const argv0 = (process.argv0 || "node").split("/").pop().replace(/\.exe$/, "");
          msg += `\n(Use \`${argv0} --trace-warnings ...\` to show where the warning was created)`;
        }
        ops.write(2, `${msg}\n`);
      });
      return;
    }
    if (process.listenerCount("warning") > 0) {
      queueMicrotask(() => process.emit("warning", w));
    } else {
      ops.write(2, `(pi:${process.pid}) ${code ? `[${code}] ` : ""}${w.name}: ${w.message}\n`);
    }
  },
  exit(code) {
    if (code !== undefined) exitCode = code;
    const c = Number(exitCode ?? 0) | 0;
    if (internal.isWorker) internal.workerExiting = true;
    try {
      process.emit("exit", c);
    } catch {}
    ops.exit(c);
    const err = new Error(`process.exit(${c}) called`);
    err.code = "ERR_PROCESS_EXIT";
    err.exitCode = c;
    throw err;
  },
  abort() {
    process.exit(134);
  },
  kill(pid, signal = "SIGTERM") {
    if (pid != (pid | 0)) {
      const err = new TypeError(`The "pid" argument must be of type number. Received ${internal.describeReceived(pid)}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    const signals = internal.signals;
    const sig = typeof signal === "number" ? signal : signals[signal];
    if (sig === undefined || (typeof signal === "number" && !Object.values(signals).includes(signal) && signal !== 0)) {
      const err = new TypeError(`Unknown signal: ${signal}`);
      err.code = "ERR_UNKNOWN_SIGNAL";
      throw err;
    }
    return ops.kill(pid, sig);
  },
  umask: () => 0o22,
  getuid: () => ops.osInfo().uid,
  geteuid: () => ops.osInfo().uid,
  getgid: () => ops.osInfo().gid,
  getegid: () => ops.osInfo().gid,
  getgroups: () => [ops.osInfo().gid],
  binding(name) {
    throw new Error(`No such module: ${name}`);
  },
  setUncaughtExceptionCaptureCallback() {},
  hasUncaughtExceptionCaptureCallback: () => false,
  setSourceMapsEnabled() {},
  getBuiltinModule(id) {
    const name = String(id).replace(/^node:/, "");
    return internal.hasBuiltin(name) ? internal.requireBuiltin(name) : undefined;
  },
  emit: EventEmitter.prototype.emit,
});
{
  let stdin;
  Object.defineProperty(process, "stdin", {
    get: () => (stdin ??= config.stdin ? makeReadableStdin() : makeStdin()),
    set: (v) => {
      stdin = v;
    },
    enumerable: true,
    configurable: true,
  });
}
Object.defineProperty(process, "exitCode", {
  get: () => exitCode,
  set: (v) => {
    exitCode = v;
  },
  enumerable: true,
  configurable: true,
});
process.report = { getReport: () => ({ header: { glibcVersionRuntime: undefined } }) };
Object.defineProperty(process, Symbol.toStringTag, { value: "process", enumerable: false, configurable: true, writable: true });

internal.signals = {
  SIGHUP: 1,
  SIGINT: 2,
  SIGQUIT: 3,
  SIGILL: 4,
  SIGTRAP: 5,
  SIGABRT: 6,
  SIGIOT: 6,
  SIGBUS: 7,
  SIGFPE: 8,
  SIGKILL: 9,
  SIGUSR1: 10,
  SIGSEGV: 11,
  SIGUSR2: 12,
  SIGPIPE: 13,
  SIGALRM: 14,
  SIGTERM: 15,
  SIGCHLD: 17,
  SIGSTKFLT: 16,
  SIGCONT: 18,
  SIGSTOP: 19,
  SIGTSTP: 20,
  SIGTTIN: 21,
  SIGTTOU: 22,
  SIGURG: 23,
  SIGXCPU: 24,
  SIGXFSZ: 25,
  SIGVTALRM: 26,
  SIGPROF: 27,
  SIGWINCH: 28,
  SIGIO: 29,
  SIGPOLL: 29,
  SIGPWR: 30,
  SIGSYS: 31,
};
if (config.platform === "darwin") {
  Object.assign(internal.signals, {
    SIGBUS: 10, SIGUSR1: 30, SIGSEGV: 11, SIGUSR2: 31, SIGCHLD: 20, SIGCONT: 19, SIGSTOP: 17, SIGTSTP: 18,
    SIGURG: 16, SIGIO: 23, SIGSYS: 12, SIGINFO: 29,
  });
  for (const name of ["SIGSTKFLT", "SIGPOLL", "SIGPWR"]) delete internal.signals[name];
}

// ---------------------------------------------------------------------------
// Uncaught exceptions / unhandled rejections (offered by the engine before it reports them)
// ---------------------------------------------------------------------------

let nodeMode = false;
let warnedOnce = false;

function unhandledRejectionError(reason) {
  // V8's side-effect-free ToString: primitives as strings, objects as `#<Constructor>`.
  let shown;
  if (reason !== null && (typeof reason === "object" || typeof reason === "function")) {
    let name = "Object";
    try {
      name = Object.getPrototypeOf(reason)?.constructor?.name || "Object";
    } catch {}
    shown = typeof reason === "function" ? String(reason) : `#<${name}>`;
  } else {
    shown = typeof reason === "symbol" ? reason.toString() : String(reason);
  }
  const err = new Error(
    "This error originated either by throwing inside of an async function without a catch block, " +
      `or by rejecting a promise which was not handled with .catch(). The promise rejected with the reason "${shown}".`,
  );
  err.name = "UnhandledPromiseRejection";
  err.code = "ERR_UNHANDLED_REJECTION";
  return err;
}

// Node's `[file]:[line]\n<source>\n   ^\n` header for the frame an error was thrown at.
function sourceContext(err) {
  if (!(err instanceof Error) || typeof err.stack !== "string") return "";
  const m = /\n\s+at (?:.*\()?(\/[^():]+):(\d+):(\d+)\)?/.exec(err.stack);
  if (!m) return "";
  try {
    const text = internal.requireBuiltin("fs").readFileSync(m[1], "utf8");
    const line = text.split("\n")[Number(m[2]) - 1];
    if (line === undefined) return "";
    return `${m[1]}:${m[2]}\n${line}\n${" ".repeat(Math.max(0, Number(m[3]) - 1))}^\n\n`;
  } catch {
    return "";
  }
}

// Print an error the way Node does before exiting on it, run 'exit' listeners and exit(1).
function fatal(err) {
  let text;
  try {
    text = err instanceof Error ? internal.inspect(err) : `Uncaught ${internal.inspect(err)}`;
  } catch {
    text = String(err);
  }
  ops.write(2, `${sourceContext(err)}${text}\n\nNode.js ${process.version}\n`);
  process.exitCode = 1;
  try {
    process.emit("exit", 1);
  } catch {}
  ops.exit(1);
}
internal.fatal = fatal;

internal.onUncaught = (kind, err, promise) => {
  // After process.exit() in a worker, the ERR_PROCESS_EXIT unwinding is not an error.
  if (internal.workerExiting) return true;
  if (kind === "unhandledRejection") {
    if (process.listenerCount("unhandledRejection") > 0) {
      process.emit("unhandledRejection", err, promise);
      return true;
    }
    // Node's default `--unhandled-rejections=throw`: raise it as an uncaught exception.
    const thrown = err instanceof Error ? err : unhandledRejectionError(err);
    if (process.listenerCount("uncaughtExceptionMonitor") > 0) process.emit("uncaughtExceptionMonitor", thrown, "unhandledRejection");
    if (process.listenerCount("uncaughtException") > 0) {
      process.emit("uncaughtException", thrown, "unhandledRejection");
      return true;
    }
    if (internal.isWorker) {
      internal.workerFatal(thrown);
      return true;
    }
    if (nodeMode) {
      fatal(thrown);
      return true;
    }
    return false;
  }
  if (process.listenerCount("uncaughtExceptionMonitor") > 0) process.emit("uncaughtExceptionMonitor", err, "uncaughtException");
  if (process.listenerCount("uncaughtException") > 0) {
    process.emit("uncaughtException", err, "uncaughtException");
    return true;
  }
  if (internal.isWorker) {
    internal.workerFatal(err);
    return true;
  }
  if (nodeMode) {
    fatal(err);
    return true;
  }
  return false;
};

// ---------------------------------------------------------------------------
// Node mode (src/node.rs): main module, beforeExit/exit
// ---------------------------------------------------------------------------

internal.nodeMain = ({ file, eval: code, print, ipcFd, serialization, uniqueId }) => {
  const done = nodeMainAsync({ file, eval: code, print, ipcFd, serialization, uniqueId });
  // Report a synchronous main as done right away (no promise to await).
  return done;
};
const nodeMainAsync = async ({ file, eval: code, print, ipcFd, serialization, uniqueId }) => {
  nodeMode = true;
  if (uniqueId !== null && uniqueId !== undefined) internal.clusterUniqueId = uniqueId;
  if (ipcFd !== null && ipcFd !== undefined) {
    internal.loadGlue("node/child_process.js");
    internal.setupProcessChannel(ipcFd, serialization);
    // A cluster worker announces itself to the primary at startup (cluster._setupWorker).
    if (internal.clusterUniqueId !== undefined) internal.loadGlue("node/cluster.js");
  }
  try {
    if (code !== null && code !== undefined) {
      const result = ops.evalScript(code, "[eval]");
      if (print) console.log(result);
    } else if (file) {
      internal.mainFilename = file;
      // Like Node, `require` is a module-scope binding of CommonJS modules, not a global.
      const require = globalThis.require;
      delete globalThis.require;
      // A CommonJS main module runs synchronously (ticks before promise jobs afterwards).
      if (ops.isCommonJs(file)) require(file);
      else await import(file);
    }
  } catch (err) {
    fatal(err);
  }
  process.channel?.release?.();
};

internal.nodeBeforeExit = () => {
  process.emit("beforeExit", Number(process.exitCode ?? 0) | 0);
  return ops.loopAlive();
};

internal.nodeExit = () => {
  const code = Number(process.exitCode ?? 0) | 0;
  process._exiting = true;
  try {
    process.emit("exit", code);
  } catch (err) {
    fatal(err);
  }
  return Number(process.exitCode ?? code) | 0;
};

// ---------------------------------------------------------------------------
// Signals (src/ops/signal.rs): listening starts with the first listener, like Node's
// startListeningIfSignal / stopListeningIfSignal.
// ---------------------------------------------------------------------------

internal.signalEvent = (signo) => {
  for (const [name, no] of Object.entries(internal.signals)) {
    if (no === signo && process.listenerCount(name) > 0) process.emit(name, name, signo);
  }
};
process.on("newListener", (type) => {
  if (typeof type !== "string" || !Object.prototype.hasOwnProperty.call(internal.signals, type)) return;
  if (internal.isWorker) return; // Signals are not delivered to workers.
  if (process.listenerCount(type) === 0) ops.signalSubscribe(internal.signals[type]);
});
process.on("removeListener", (type) => {
  if (typeof type !== "string" || !Object.prototype.hasOwnProperty.call(internal.signals, type)) return;
  if (internal.isWorker) return;
  if (process.listenerCount(type) === 0) ops.signalUnsubscribe(internal.signals[type]);
});

// `require` for classic scripts (like `node -e`), relative to the cwd.
internal.hidden(globalThis, "require", internal.makeRequire(`${ops.cwd()}/[eval]`));

internal.process = process;
internal.hidden(globalThis, "process", process);
internal.hidden(globalThis, "global", globalThis);
internal.defineBuiltin("process", () => process);
