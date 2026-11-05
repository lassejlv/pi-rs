// node:child_process

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;
const children = new Map();

function normalizeStdio(stdio) {
  if (stdio === undefined || stdio === null) return ["pipe", "pipe", "pipe"];
  if (typeof stdio === "string") {
    if (stdio === "overlapped") return ["pipe", "pipe", "pipe"];
    if (!["pipe", "inherit", "ignore"].includes(stdio)) throw invalidArgValue("options.stdio", stdio);
    return [stdio, stdio, stdio];
  }
  if (!Array.isArray(stdio)) throw invalidArgValue("options.stdio", stdio);
  const out = [];
  let ipcSeen = false;
  for (let i = 0; i < Math.max(3, stdio.length); i++) {
    const s = stdio[i];
    if (s === "ipc") {
      if (ipcSeen) {
        const err = new Error("Child process can have only one IPC pipe");
        err.code = "ERR_IPC_ONE_PIPE";
        throw err;
      }
      ipcSeen = true;
      out.push("ipc");
    } else if (i < 3 && (s === undefined || s === null)) out.push("pipe");
    else if (s === undefined || s === null || s === "ignore") out.push("ignore");
    else if (s === "pipe" || s === "overlapped") out.push("pipe");
    else if (s === "inherit" || s === i || (i < 3 && (s === process.stdin || s === process.stdout || s === process.stderr))) out.push(i < 3 ? "inherit" : "ignore");
    else if (i < 3 && typeof s === "number" && s >= 0 && s <= 2) out.push("inherit");
    else out.push("pipe");
  }
  return out;
}

function invalidArgValue(name, value) {
  const err = new TypeError(`The argument '${name}' is invalid. Received ${internal.inspect(value)}`);
  err.code = "ERR_INVALID_ARG_VALUE";
  return err;
}

function envPairs(env) {
  const out = [];
  for (const [k, v] of Object.entries(env ?? process.env)) {
    if (v === undefined) continue;
    out.push(k, String(v));
  }
  return out;
}

function resolveCwd(cwd) {
  if (cwd === undefined || cwd === null) return process.cwd();
  if (cwd instanceof internal.URL) return decodeURIComponent(cwd.pathname);
  return internal.requireBuiltin("path").resolve(String(cwd));
}

function normalizeSpawnArgs(command, args, options) {
  if (!Array.isArray(args)) {
    options = args ?? {};
    args = [];
  }
  options = { ...(options ?? {}) };
  let file = String(command);
  let argv = args.map(String);
  if (options.shell) {
    const cmd = [file, ...argv].join(" ");
    file = typeof options.shell === "string" ? options.shell : "/bin/sh";
    argv = ["-c", cmd];
  }
  return { file, args: argv, options };
}

function spawnError(file, args, code, errno, syscall = "spawn") {
  const err = new Error(`${syscall} ${file} ${code}`);
  err.errno = errno;
  err.code = code;
  err.syscall = `${syscall} ${file}`;
  err.path = file;
  err.spawnargs = args;
  return err;
}

class ChildProcess extends EventEmitter {
  constructor() {
    super();
    this.pid = undefined;
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this.connected = false;
    this.stdin = null;
    this.stdout = null;
    this.stderr = null;
    this.stdio = [null, null, null];
    this.spawnfile = undefined;
    this.spawnargs = [];
  }
  kill(signal = "SIGTERM") {
    if (this._id === undefined || this.exitCode !== null || this.signalCode !== null) return false;
    const sig = typeof signal === "number" ? signal : internal.signals[signal];
    if (sig === undefined) {
      const err = new TypeError(`Unknown signal: ${signal}`);
      err.code = "ERR_UNKNOWN_SIGNAL";
      throw err;
    }
    const ok = ops.childKill(this._id, sig);
    if (ok) this.killed = true;
    return ok;
  }
  ref() {
    if (this._id !== undefined && !this._refed && this._alive) {
      this._refed = true;
      ops.loopRef(1);
    }
    return this;
  }
  unref() {
    if (this._refed) {
      this._refed = false;
      ops.loopRef(-1);
    }
    return this;
  }
  [Symbol.dispose]() {
    this.kill();
  }
}

internal.childEvent = (id, kind, a, b) => {
  const child = children.get(id);
  if (!child) return;
  switch (kind) {
    case "stdout":
      child.stdout?.push(Buffer.from(a.buffer, a.byteOffset, a.byteLength));
      break;
    case "stderr":
      child.stderr?.push(Buffer.from(a.buffer, a.byteOffset, a.byteLength));
      break;
    case "exit": {
      const emitExit = () => {
        child.exitCode = a;
        child.signalCode = b;
        if (child._timeout) clearTimeout(child._timeout);
        // Node destroys the stdin pipe when the child exits.
        if (child.stdin && !child.stdin.destroyed) child.stdin.destroy();
        child.emit("exit", a, b);
        child._exitEmitted = true;
        if (child._pendingClose) child._pendingClose();
      };
      // The IPC channel of an exited child reaches EOF right away; report the disconnect first,
      // in the order Node's event loop observes them.
      if (child.connected) child.once("disconnect", () => process.nextTick(emitExit));
      else emitExit();
      break;
    }
    case "close": {
      const emitClose = () => {
        // The process and its stdio kept the loop alive until now (Node's handle + pipes).
        child._alive = false;
        child.unref();
        children.delete(id);
        child.stdout?.push(null);
        child.stderr?.push(null);
        // Let stream 'end' events flush before 'close'.
        setImmediate(() => child.emit("close", a, b));
      };
      if (child._exitEmitted) emitClose();
      else child._pendingClose = emitClose;
      break;
    }
  }
};

function spawn(command, args, options) {
  const n = normalizeSpawnArgs(command, args, options);
  const opts = n.options;
  const { Readable, Writable } = internal.requireBuiltin("stream");
  const child = new ChildProcess();
  const stdio = normalizeStdio(opts.stdio);
  child.spawnfile = n.file;
  child.spawnargs = [opts.argv0 ?? n.file, ...n.args];
  if (opts.signal?.aborted) {
    process.nextTick(() => child.emit("error", Object.assign(new internal.DOMException("The operation was aborted", "AbortError"), { code: "ABORT_ERR" })));
    return child;
  }
  const extra = [];
  for (let i = 3; i < stdio.length; i++) if (stdio[i] === "pipe" || stdio[i] === "ipc") extra.push(i);
  const ipcIndex = stdio.indexOf("ipc");
  let env = opts.env ?? process.env;
  if (ipcIndex >= 0) {
    const serialization = opts.serialization ?? "json";
    if (serialization !== "json" && serialization !== "advanced") {
      const err = new TypeError(`The property 'options.serialization' must be one of: 'json', 'advanced'. Received ${internal.inspect(serialization)}`);
      err.code = "ERR_INVALID_ARG_VALUE";
      throw err;
    }
    env = { ...env, NODE_CHANNEL_FD: String(ipcIndex), NODE_CHANNEL_SERIALIZATION_MODE: serialization };
    if (ipcIndex < 3) extra.push(ipcIndex);
  }
  const res = ops.spawn(n.file, n.args, {
    cwd: resolveCwd(opts.cwd),
    env: envPairs(env),
    stdio: stdio.slice(0, 3).map((m) => (m === "ipc" ? "ignore" : m)),
    detached: Boolean(opts.detached),
    argv0: opts.argv0,
    extra,
    uid: opts.uid,
    gid: opts.gid,
  });
  if (res.error) {
    process.nextTick(() => {
      child.emit("error", spawnError(n.file, n.args, res.code, res.errno));
      child.emit("close", -2, null);
    });
    return child;
  }
  child._id = res.id;
  child.pid = res.pid;
  child._alive = true;
  child._refed = false;
  child.ref();
  children.set(res.id, child);
  const extraIds = new Map();
  for (let i = 0; i < res.extra.length; i += 2) extraIds.set(res.extra[i], res.extra[i + 1]);
  if (stdio[0] === "pipe") {
    child.stdin = new Writable({
      write(chunk, encoding, cb) {
        if (!ops.childWrite(res.id, chunk)) {
          const err = new Error("write EPIPE");
          err.code = "EPIPE";
          err.syscall = "write";
          cb(err);
          return;
        }
        cb();
      },
      final(cb) {
        ops.childCloseStdin(res.id);
        cb();
      },
      destroy(err, cb) {
        ops.childCloseStdin(res.id);
        cb(err);
      },
    });
    child.stdin.on("error", () => {});
  }
  if (stdio[1] === "pipe") child.stdout = new Readable({ read() {} });
  if (stdio[2] === "pipe") child.stderr = new Readable({ read() {} });
  child.stdio = [child.stdin, child.stdout, child.stderr];
  for (let i = 3; i < stdio.length; i++) {
    child.stdio[i] = stdio[i] === "pipe" ? pipeStream(extraIds.get(i)) : null;
  }
  if (ipcIndex >= 0) setupChannel(child, extraIds.get(ipcIndex), opts.serialization ?? "json");
  if (opts.timeout > 0) {
    child._timeout = setTimeout(() => child.kill(opts.killSignal ?? "SIGTERM"), opts.timeout);
  }
  if (opts.signal) {
    const onAbort = () => {
      child.kill(opts.killSignal ?? "SIGTERM");
      const err = new internal.DOMException("The operation was aborted", "AbortError");
      err.code = "ABORT_ERR";
      err.cause = opts.signal.reason;
      child.emit("error", err);
    };
    opts.signal.addEventListener("abort", onAbort, { once: true });
    child.once("exit", () => opts.signal.removeEventListener("abort", onAbort));
  }
  process.nextTick(() => child.emit("spawn"));
  return child;
}

function collectOutput(child, options, finish) {
  const encoding = options.encoding === undefined ? "utf8" : options.encoding;
  const maxBuffer = options.maxBuffer ?? 1024 * 1024;
  const out = [];
  const err = [];
  let outLen = 0;
  let errLen = 0;
  let killedForBuffer = false;
  child.stdout?.on("data", (c) => {
    outLen += c.length;
    if (outLen > maxBuffer && !killedForBuffer) {
      killedForBuffer = true;
      child.kill(options.killSignal ?? "SIGTERM");
    }
    out.push(c);
  });
  child.stderr?.on("data", (c) => {
    errLen += c.length;
    if (errLen > maxBuffer && !killedForBuffer) {
      killedForBuffer = true;
      child.kill(options.killSignal ?? "SIGTERM");
    }
    err.push(c);
  });
  const conv = (chunks) => {
    const b = Buffer.concat(chunks);
    return encoding && encoding !== "buffer" ? b.toString(encoding) : b;
  };
  let spawnErr = null;
  child.on("error", (e) => {
    spawnErr = e;
  });
  child.on("close", (code, signal) => finish(spawnErr, code, signal, conv(out), conv(err), killedForBuffer));
}

function makeExecError(cmd, code, signal, stdout, stderr, killed) {
  const err = new Error(`Command failed: ${cmd}\n${typeof stderr === "string" ? stderr : stderr.toString()}`);
  err.code = code;
  err.killed = killed;
  err.signal = signal;
  err.cmd = cmd;
  err.stdout = stdout;
  err.stderr = stderr;
  return err;
}

function exec(command, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = { ...(options ?? {}) };
  const child = spawn(command, [], { ...options, shell: options.shell ?? true });
  collectOutput(child, options, (spawnErr, code, signal, stdout, stderr, bufferKill) => {
    if (!callback) return;
    if (spawnErr) {
      spawnErr.stdout = stdout;
      spawnErr.stderr = stderr;
      callback(spawnErr, stdout, stderr);
    } else if (code !== 0 || signal) {
      const err = makeExecError(command, code, signal, stdout, stderr, child.killed || bufferKill);
      if (bufferKill) err.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
      callback(err, stdout, stderr);
    } else callback(null, stdout, stderr);
  });
  return child;
}

function execFile(file, args, options, callback) {
  if (typeof args === "function") {
    callback = args;
    args = [];
    options = {};
  } else if (!Array.isArray(args)) {
    callback = options;
    options = args ?? {};
    args = [];
  }
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = { ...(options ?? {}) };
  const child = spawn(file, args, options);
  const cmd = [file, ...args].join(" ");
  collectOutput(child, options, (spawnErr, code, signal, stdout, stderr, bufferKill) => {
    if (!callback) return;
    if (spawnErr) {
      spawnErr.stdout = stdout;
      spawnErr.stderr = stderr;
      callback(spawnErr, stdout, stderr);
    } else if (code !== 0 || signal) callback(makeExecError(cmd, code, signal, stdout, stderr, child.killed || bufferKill), stdout, stderr);
    else callback(null, stdout, stderr);
  });
  return child;
}

const promisifyExec = (fn) =>
  function (...args) {
    let child;
    const p = new Promise((resolve, reject) => {
      child = fn(...args, (err, stdout, stderr) => {
        if (err) {
          err.stdout = stdout;
          err.stderr = stderr;
          reject(err);
        } else resolve({ stdout, stderr });
      });
    });
    p.child = child;
    return p;
  };
exec[Symbol.for("nodejs.util.promisify.custom")] = promisifyExec(exec);
execFile[Symbol.for("nodejs.util.promisify.custom")] = promisifyExec(execFile);

function spawnSync(command, args, options) {
  const n = normalizeSpawnArgs(command, args, options);
  const opts = n.options;
  const stdio = normalizeStdio(opts.stdio);
  let input = opts.input;
  if (typeof input === "string") input = Buffer.from(input, opts.encoding && opts.encoding !== "buffer" ? opts.encoding : "utf8");
  const killSignal = typeof opts.killSignal === "number" ? opts.killSignal : internal.signals[opts.killSignal ?? "SIGTERM"];
  const r = ops.spawnSync(n.file, n.args, { cwd: resolveCwd(opts.cwd), env: envPairs(opts.env), stdio }, input ?? null, opts.timeout ?? 0, killSignal);
  const encoding = opts.encoding && opts.encoding !== "buffer" ? opts.encoding : null;
  const conv = (u8) => {
    if (u8 === undefined) return null;
    const b = Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
    return encoding ? b.toString(encoding) : b;
  };
  if (r.error && r.code !== "ETIMEDOUT") {
    return {
      pid: 0,
      output: null,
      stdout: null,
      stderr: null,
      status: null,
      signal: null,
      error: spawnError(n.file, n.args, r.code, r.errno, "spawnSync"),
    };
  }
  const stdout = stdio[1] === "pipe" ? conv(r.stdout) : null;
  const stderr = stdio[2] === "pipe" ? conv(r.stderr) : null;
  const result = { pid: r.pid, output: [null, stdout, stderr], stdout, stderr, status: r.status, signal: r.signal };
  if (r.error) result.error = spawnError(n.file, n.args, "ETIMEDOUT", -110, "spawnSync");
  return result;
}

function checkSync(result, cmd) {
  if (result.error) {
    result.error.stdout = result.stdout;
    result.error.stderr = result.stderr;
    throw result.error;
  }
  if (result.status !== 0 || result.signal) {
    const stderr = result.stderr ?? "";
    const err = new Error(`Command failed: ${cmd}${stderr.length ? `\n${stderr.toString()}` : ""}`);
    Object.assign(err, { status: result.status, signal: result.signal, output: result.output, pid: result.pid, stdout: result.stdout, stderr: result.stderr });
    throw err;
  }
  return result.stdout;
}

function execSync(command, options = {}) {
  const stdio = options.stdio ?? ["pipe", "pipe", "inherit"];
  const result = spawnSync(command, [], { ...options, stdio, shell: options.shell ?? true });
  return checkSync(result, command);
}

function execFileSync(file, args, options) {
  if (!Array.isArray(args)) {
    options = args ?? {};
    args = [];
  }
  options = options ?? {};
  const stdio = options.stdio ?? ["pipe", "pipe", "inherit"];
  const result = spawnSync(file, args, { ...options, stdio });
  return checkSync(result, [file, ...args].join(" "));
}

// ---------------------------------------------------------------------------
// Extra stdio pipes and the IPC channel (src/ops/ipc.rs)
// ---------------------------------------------------------------------------

const channelHandlers = new Map();
internal.ipcEvent = (id, kind, a) => channelHandlers.get(id)?.[kind]?.(a);

function pipeStream(id) {
  const { Duplex } = internal.requireBuiltin("stream");
  const stream = new Duplex({
    read() {},
    write(chunk, encoding, cb) {
      ops.ipcWrite(id, chunk);
      cb();
    },
    final(cb) {
      ops.ipcClose(id);
      cb();
    },
  });
  channelHandlers.set(id, {
    data: (u8) => stream.push(Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength)),
    fd: (fd) => ops.closeFd(fd),
    close: () => {
      channelHandlers.delete(id);
      stream.push(null);
    },
  });
  return stream;
}

function nodeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

// Node's `setupChannel` (lib/internal/child_process.js) for both ends: `target` is the
// ChildProcess in the parent or `process` in the child.
function setupChannel(target, id, serialization) {
  const advanced = serialization === "advanced";
  if (advanced) internal.requireBuiltin("v8");
  let text = "";
  let bytes = Buffer.alloc(0);
  const decoder = new internal.TextDecoder("utf-8");
  const pendingFds = [];
  let refs = 0;
  let refed = false;
  const updateRef = () => {
    const want = target.connected && refs > 0;
    if (want !== refed) {
      refed = want;
      ops.loopRef(want ? 1 : -1);
    }
  };
  const channel = {
    fd: id,
    ref() {
      refs++;
      updateRef();
    },
    unref() {
      refs = Math.max(0, refs - 1);
      updateRef();
    },
    refCounted() {
      refs++;
      updateRef();
    },
    unrefCounted() {
      refs = Math.max(0, refs - 1);
      updateRef();
    },
    get sockets() {
      return { got: {}, send: {} };
    },
  };
  target.channel = channel;
  target._channel = channel;
  target.connected = true;
  target._handleQueue = null;

  // The child holds messages back until its main module has been evaluated (Node runs a CommonJS
  // main synchronously before reading the channel; our module loading spans event loop turns).
  const held = [];
  channel.hold = () => {
    channel.held = true;
  };
  channel.release = () => {
    channel.held = false;
    process.nextTick(() => {
      for (const m of held.splice(0)) deliver(m);
    });
  };
  const emitMessage = (message, handle) => {
    if (message !== null && typeof message === "object" && typeof message.cmd === "string" && message.cmd.startsWith("NODE_")) {
      target.emit("internalMessage", message, handle);
    } else {
      target.emit("message", message, handle);
    }
  };
  const deliver = (message) => {
    if (channel.held) {
      held.push(message);
      return;
    }
    // A message carrying a handle (Node's NODE_HANDLE protocol): acknowledge, rebuild the
    // handle from the received descriptor, then deliver the wrapped message with it.
    if (message !== null && typeof message === "object" && message.cmd === "NODE_HANDLE") {
      const fd = pendingFds.shift();
      if (target.connected) ops.ipcWrite(id, advanced ? internal.ipcFrame({ cmd: "NODE_HANDLE_ACK" }) : JSON.stringify({ cmd: "NODE_HANDLE_ACK" }) + "\n");
      target.emit("internalMessage", message);
      internal.ipcHandleGot(message, fd, (handle) => emitMessage(message.msg, handle));
      return;
    }
    emitMessage(message, undefined);
  };

  const finish = () => {
    if (!target.connected && !target._disconnectEmitted) {
      target._disconnectEmitted = true;
      updateRef();
      channelHandlers.delete(id);
      process.nextTick(() => target.emit("disconnect"));
    }
  };

  channelHandlers.set(id, {
    fd: (fd) => pendingFds.push(fd),
    data(u8) {
      if (advanced) {
        bytes = bytes.length ? Buffer.concat([bytes, Buffer.from(u8)]) : Buffer.from(u8);
        while (bytes.length >= 4) {
          const size = bytes.readUInt32BE(0);
          if (bytes.length < 4 + size) break;
          const message = internal.ipcDeserialize(bytes.subarray(4, 4 + size));
          bytes = bytes.subarray(4 + size);
          deliver(message);
        }
        return;
      }
      text += decoder.decode(u8, { stream: true });
      let nl;
      while ((nl = text.indexOf("\n")) >= 0) {
        const line = text.slice(0, nl);
        text = text.slice(nl + 1);
        deliver(JSON.parse(line));
      }
    },
    close() {
      for (const fd of pendingFds.splice(0)) ops.closeFd(fd);
      if (target.connected) {
        target.connected = false;
        ops.ipcClose(id);
      }
      finish();
    },
  });

  target.send = function send(message, handle, options, callback) {
    if (typeof handle === "function") {
      callback = handle;
      handle = undefined;
      options = undefined;
    } else if (typeof options === "function") {
      callback = options;
      options = undefined;
    } else if (options !== undefined && (options === null || typeof options !== "object")) {
      throw nodeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "options" argument must be of type object. Received ${internal.describeReceived?.(options) ?? typeof options}`);
    }
    options = { swallowErrors: false, ...options };
    if (this.connected) return this._send(message, handle, options, callback);
    const ex = nodeError(Error, "ERR_IPC_CHANNEL_CLOSED", "Channel closed");
    if (typeof callback === "function") process.nextTick(callback, ex);
    else process.nextTick(() => this.emit("error", ex));
    return false;
  };

  target._send = function _send(message, handle, options, callback) {
    if (message === undefined) throw nodeError(TypeError, "ERR_MISSING_ARGS", 'The "message" argument must be specified');
    if (typeof message !== "string" && typeof message !== "object" && typeof message !== "number" && typeof message !== "boolean") {
      throw nodeError(
        TypeError,
        "ERR_INVALID_ARG_TYPE",
        `The "message" argument must be one of type string, object, number, or boolean. Received ${internal.describeReceived?.(message) ?? typeof message}`,
      );
    }
    let fd;
    if (handle !== undefined && handle !== null) {
      const packed = internal.ipcPackHandle(message, handle, options);
      if (!packed) {
        throw nodeError(TypeError, "ERR_INVALID_HANDLE_TYPE", "This handle type cannot be sent");
      }
      message = packed.message;
      fd = packed.fd;
      this._packedAfter = packed.after;
    }
    let after;
    if (handle !== undefined && handle !== null) after = this._packedAfter;
    const payload = advanced ? internal.ipcFrame(message) : JSON.stringify(message) + "\n";
    const ok = ops.ipcWrite(id, payload, fd);
    after?.();
    if (typeof callback === "function") process.nextTick(callback, ok ? null : nodeError(Error, "ERR_IPC_CHANNEL_CLOSED", "Channel closed"));
    return true;
  };

  target.disconnect = function disconnect() {
    if (!this.connected) {
      this.emit("error", nodeError(Error, "ERR_IPC_DISCONNECTED", "IPC channel is already disconnected"));
      return;
    }
    this.connected = false;
    ops.ipcClose(id);
    finish();
  };
  target._disconnect = target.disconnect;
  updateRef();
  return channel;
}
internal.setupChannel = setupChannel;

internal.ipcFrame = (message) => {
  const body = internal.ipcSerialize(message);
  const payload = Buffer.alloc(4 + body.length);
  payload.writeUInt32BE(body.length, 0);
  body.copy(payload, 4);
  return payload;
};

// Handles that can be sent with `send(message, handle)` (Node's handleConversion).
internal.ipcPackHandle = (message, handle, options = {}) => {
  const net = internal.requireBuiltin("net");
  let type;
  let fd = -1;
  let after;
  if (handle instanceof net.Server) {
    type = "net.Server";
    fd = handle._id !== undefined ? ops.netFd(handle._id) : -1;
  } else if (handle instanceof net.Socket) {
    type = "net.Socket";
    fd = handle._id !== undefined ? ops.netFd(handle._id) : -1;
    if (!options.keepOpen) {
      after = () => {
        // The descriptor now belongs to the receiver: stop here without shutting it down.
        ops.netDetach(handle._id);
        handle._detached = true;
        handle.destroy();
      };
    }
  } else if (handle && handle._fdHandle) {
    type = "net.Native";
    fd = handle.fd;
  } else {
    return null;
  }
  if (fd < 0) return null;
  return { message: { cmd: "NODE_HANDLE", type, msg: message }, fd, after };
};

internal.ipcHandleGot = (message, fd, emit) => {
  if (fd === undefined) {
    emit(undefined);
    return;
  }
  const net = internal.requireBuiltin("net");
  switch (message.type) {
    case "net.Server": {
      const server = new net.Server();
      server.listen({ fd }, () => emit(server));
      break;
    }
    case "net.Socket":
      emit(new net.Socket({ fd, readable: true, writable: true }));
      break;
    case "net.Native":
      emit({ fd, _fdHandle: true, close: () => ops.closeFd(fd) });
      break;
    default:
      ops.closeFd(fd);
      emit(undefined);
  }
};

// The child's end (`process.send` & co.), set up at startup in Node mode (src/node.rs).
internal.setupProcessChannel = (fd, serialization) => {
  const id = ops.ipcOpen(fd);
  const channel = setupChannel(process, id, serialization);
  channel.hold();
  // Like Node: the channel keeps the process alive while it has 'message'/'disconnect' listeners.
  process.on("newListener", (name) => {
    if (name === "message" || name === "disconnect") channel.refCounted();
  });
  process.on("removeListener", (name) => {
    if (name === "message" || name === "disconnect") channel.unrefCounted();
  });
};

function fork(modulePath, args, options) {
  if (modulePath instanceof internal.URL) modulePath = internal.requireBuiltin("url").fileURLToPath(modulePath);
  if (typeof modulePath !== "string") {
    throw nodeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "modulePath" argument must be of type string or an instance of Buffer or URL. Received ${internal.describeReceived?.(modulePath) ?? typeof modulePath}`);
  }
  if (args == null) args = [];
  else if (typeof args === "object" && !Array.isArray(args)) {
    options = args;
    args = [];
  } else if (!Array.isArray(args)) {
    throw nodeError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'args' is invalid. Received ${internal.inspect(args)}`);
  }
  options = { ...(options ?? {}) };
  const execArgv = options.execArgv ?? process.execArgv;
  if (execArgv === process.execArgv && process._eval != null) {
    const index = execArgv.lastIndexOf(process._eval);
    if (index > 0) execArgv.splice(index - 1, 2);
  }
  args = [...execArgv, modulePath, ...args];
  if (typeof options.stdio === "string") {
    options.stdio = options.stdio === "ignore" ? ["ignore", "ignore", "ignore", "ipc"] : [options.stdio, options.stdio, options.stdio, "ipc"];
  } else if (!Array.isArray(options.stdio)) {
    options.stdio = options.silent ? ["pipe", "pipe", "pipe", "ipc"] : ["inherit", "inherit", "inherit", "ipc"];
  } else if (!options.stdio.includes("ipc")) {
    throw nodeError(Error, "ERR_CHILD_PROCESS_IPC_REQUIRED", "Forked processes must have an IPC channel, missing value 'ipc' in options.stdio");
  }
  options.shell = false;
  // Without a node binary the child is another pi-jsrt runtime: this executable in Node mode.
  let file = options.execPath;
  if (file === undefined || file === process.execPath) {
    const [program, ...prefix] = ops.config.nodeExec;
    file = program;
    args = [...prefix, ...args];
  }
  return spawn(file, args, options);
}

const cp = { spawn, exec, execFile, spawnSync, execSync, execFileSync, fork, ChildProcess };
internal.defineBuiltin("child_process", () => cp);
