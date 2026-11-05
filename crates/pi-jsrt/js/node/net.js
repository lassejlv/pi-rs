// node:net — TCP sockets and servers over tokio (ops/net.rs). Unix domain sockets and IPC
// pipes are not supported.

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;
const { Duplex } = internal.requireBuiltin("stream");

// id -> Socket | Server
const handles = new Map();

internal.netEvent = (id, kind, a, b) => {
  const target = handles.get(id);
  if (!target) return;
  target._onNetEvent(kind, a, b);
};

function netError(info) {
  const err = new Error(info.message);
  for (const key of ["code", "errno", "syscall", "address", "port"]) {
    if (info[key] !== undefined) err[key] = info[key];
  }
  return err;
}

function isIPv4(input) {
  const parts = String(input).split(".");
  return parts.length === 4 && parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) <= 255 && (p === "0" || !p.startsWith("0")));
}

function isIPv6(input) {
  let addr = String(input).split("%")[0];
  let extra = 0;
  const lastColon = addr.lastIndexOf(":");
  if (addr.slice(lastColon + 1).includes(".")) {
    if (!isIPv4(addr.slice(lastColon + 1))) return false;
    addr = addr.slice(0, lastColon + 1) + "0";
    extra = 1;
  }
  const halves = addr.split("::");
  if (halves.length > 2) return false;
  const groups = halves.flatMap((half) => (half === "" ? [] : half.split(":")));
  if (!groups.every((g) => /^[0-9a-fA-F]{1,4}$/.test(g))) return false;
  const count = groups.length + extra;
  return halves.length === 2 ? count <= 7 : count === 8;
}

function isIP(input) {
  if (isIPv4(input)) return 4;
  if (isIPv6(input)) return 6;
  return 0;
}

function normalizeConnectArgs(args) {
  let [first, second, third] = args;
  let options;
  let callback;
  if (first !== null && typeof first === "object") {
    options = { ...first };
    callback = typeof second === "function" ? second : undefined;
  } else if (typeof first === "string" && !/^\d+$/.test(first)) {
    throw Object.assign(new Error("Unix domain sockets are not supported in pi-jsrt"), { code: "ERR_NOT_SUPPORTED" });
  } else {
    options = { port: first };
    if (typeof second === "string") {
      options.host = second;
      callback = typeof third === "function" ? third : undefined;
    } else {
      callback = typeof second === "function" ? second : undefined;
    }
  }
  if (options.path !== undefined) {
    throw Object.assign(new Error("Unix domain sockets are not supported in pi-jsrt"), { code: "ERR_NOT_SUPPORTED" });
  }
  return { options, callback };
}

class Socket extends Duplex {
  constructor(options = {}) {
    super({ allowHalfOpen: options.allowHalfOpen === true, decodeStrings: true });
    this.allowHalfOpen = options.allowHalfOpen === true;
    this._id = undefined;
    this.connecting = false;
    this.pending = true;
    this.remoteAddress = undefined;
    this.remotePort = undefined;
    this.remoteFamily = undefined;
    this.localAddress = undefined;
    this.localPort = undefined;
    this.localFamily = undefined;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this._timeoutMs = 0;
    this._timer = undefined;
    this._pendingWrites = [];
    this._loop = internal.loopHandle();
    // A socket over a passed descriptor (child_process handle passing, cluster connections).
    const fd = typeof options.fd === "number" ? options.fd : options.handle && typeof options.handle.fd === "number" ? options.handle.fd : undefined;
    if (fd !== undefined) {
      const r = ops.netAdoptFd(fd);
      if (r.error) throw netError(r.error);
      this._attach(r.id, r);
    }
  }

  _attach(id, info) {
    this._loop.activate();
    this._id = id;
    handles.set(id, this);
    this._setEndpoints(info);
    this.connecting = false;
    this.pending = false;
  }

  _setEndpoints(info) {
    if (!info) return;
    for (const key of ["remoteAddress", "remotePort", "remoteFamily", "localAddress", "localPort", "localFamily"]) {
      if (info[key] !== undefined) this[key] = info[key];
    }
  }

  get readyState() {
    if (this.connecting) return "opening";
    if (this.destroyed) return "closed";
    const readable = !this._readableState.ended;
    const writable = !this._writableState.ended;
    if (readable && writable) return "open";
    if (readable) return "readOnly";
    if (writable) return "writeOnly";
    return "closed";
  }

  connect(...args) {
    const { options, callback } = normalizeConnectArgs(args);
    const port = Number(options.port);
    if (!Number.isInteger(port) || port < 0 || port > 65535) {
      throw Object.assign(new RangeError(`Port should be >= 0 and < 65536. Received ${options.port}.`), { code: "ERR_SOCKET_BAD_PORT" });
    }
    if (callback) this.once("connect", callback);
    if (options.timeout) this.setTimeout(options.timeout);
    this.connecting = true;
    this._loop.activate();
    internal.dcPublish?.("net.client.socket", { socket: this });
    const id = ops.netConnect(String(options.host ?? "localhost"), port);
    this._id = id;
    handles.set(id, this);
    return this;
  }

  _onNetEvent(kind, a, b) {
    switch (kind) {
      case "connect":
        this._setEndpoints(a);
        this.connecting = false;
        this.pending = false;
        for (const [chunk, cb] of this._pendingWrites.splice(0)) this._write(chunk, "buffer", cb);
        this._touch();
        this.emit("connect");
        this.emit("ready");
        break;
      case "data": {
        const chunk = Buffer.from(a.buffer, a.byteOffset, a.byteLength);
        this.bytesRead += chunk.length;
        this._touch();
        this.push(chunk);
        break;
      }
      case "end":
        this.push(null);
        if (!this.allowHalfOpen && !this._writableState.ending) this.end();
        break;
      case "error":
        this.destroy(netError(a));
        break;
      case "close":
        handles.delete(this._id);
        this._clearTimer();
        this._loop.deactivate();
        this._remoteClosed = true;
        if (!this.destroyed) this.destroy();
        break;
    }
  }

  _read() {}

  _write(chunk, encoding, callback) {
    if (this.connecting || this._id === undefined) {
      this._pendingWrites.push([chunk, callback]);
      return;
    }
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding);
    if (!ops.netWrite(this._id, buf)) {
      callback(Object.assign(new Error("This socket has been ended by the other party"), { code: "EPIPE" }));
      return;
    }
    this.bytesWritten += buf.length;
    this._touch();
    callback();
  }

  _final(callback) {
    if (this._id !== undefined && !this.connecting) ops.netEnd(this._id);
    else this.once("connect", () => ops.netEnd(this._id));
    callback();
  }

  _destroy(err, callback) {
    this._clearTimer();
    this._loop.deactivate();
    if (this._id !== undefined && !this._remoteClosed && !this._detached) ops.netDestroy(this._id);
    callback(err);
  }

  address() {
    if (this.localAddress === undefined) return {};
    return { address: this.localAddress, family: this.localFamily, port: this.localPort };
  }

  setTimeout(ms, callback) {
    this._timeoutMs = Math.max(0, Number(ms) || 0);
    if (callback) {
      if (this._timeoutMs === 0) this.removeListener("timeout", callback);
      else this.once("timeout", callback);
    }
    this._touch();
    return this;
  }

  _touch() {
    this._clearTimer();
    if (this._timeoutMs > 0 && !this.destroyed) {
      this._timer = setTimeout(() => this.emit("timeout"), this._timeoutMs);
      this._timer.unref?.();
    }
  }

  _clearTimer() {
    if (this._timer) clearTimeout(this._timer);
    this._timer = undefined;
  }

  setNoDelay() {
    return this;
  }
  setKeepAlive() {
    return this;
  }
  ref() {
    this._loop.ref();
    return this;
  }
  unref() {
    this._loop.unref();
    return this;
  }
  resetAndDestroy() {
    return this.destroy();
  }
}

class Server extends EventEmitter {
  constructor(options, connectionListener) {
    super();
    if (typeof options === "function") {
      connectionListener = options;
      options = {};
    }
    this._options = options ?? {};
    this._id = undefined;
    this._address = null;
    this._connections = new Set();
    this.listening = false;
    this.maxConnections = undefined;
    this._loop = internal.loopHandle();
    if (connectionListener) this.on("connection", connectionListener);
  }

  listen(...args) {
    let port = 0;
    let host = "";
    let callback;
    for (const arg of args) {
      if (typeof arg === "function") callback = arg;
    }
    const [first, second] = args;
    let fd;
    if (first !== null && typeof first === "object" && (typeof first.fd === "number" || first._fdHandle)) {
      // listen(handle) / listen({ fd }): a passed listening descriptor.
      fd = first.fd;
    } else if (first !== null && typeof first === "object") {
      if (first.path !== undefined) {
        throw Object.assign(new Error("Unix domain sockets are not supported in pi-jsrt"), { code: "ERR_NOT_SUPPORTED" });
      }
      port = first.port ?? 0;
      host = first.host ?? "";
    } else if (typeof first === "string" && !/^\d+$/.test(first)) {
      throw Object.assign(new Error("Unix domain sockets are not supported in pi-jsrt"), { code: "ERR_NOT_SUPPORTED" });
    } else {
      if (first !== undefined && typeof first !== "function") port = first;
      if (typeof second === "string") host = second;
    }
    if (callback) this.once("listening", callback);
    // In a cluster worker, the primary owns the listening socket (cluster._getServer).
    if (fd === undefined && !this._rawMode && internal.clusterListen && !(first && typeof first === "object" && first.exclusive)) {
      if (internal.clusterListen(this, { address: host || null, port: Number(port) || 0, addressType: host && isIPv6(host) ? 6 : 4, fd: -1 })) return this;
    }
    const result = fd !== undefined ? ops.netListenFd(fd, Boolean(this._rawMode)) : ops.netListen(String(host), Number(port) || 0, Boolean(this._rawMode));
    if (result.error) {
      const err = netError(result.error);
      process.nextTick(() => this.emit("error", err));
      return this;
    }
    this._id = result.id;
    handles.set(result.id, this);
    this._address = { address: result.address, family: result.family, port: result.port };
    this.listening = true;
    this._loop.activate();
    process.nextTick(() => this.emit("listening"));
    return this;
  }

  // A connection passed as a descriptor (cluster round-robin).
  _acceptFd(fd) {
    const r = ops.netAdoptFd(fd);
    if (r.error) return false;
    this._onNetEvent("connection", r.id, r);
    return true;
  }

  _onNetEvent(kind, socketId, info) {
    if (kind === "rawconnection") {
      this.emit("rawconnection", socketId, info);
      return;
    }
    if (kind === "connection") {
      const socket = new Socket({ allowHalfOpen: this._options.allowHalfOpen === true });
      socket._attach(socketId, info);
      socket.server = this;
      this._connections.add(socket);
      socket.once("close", () => {
        this._connections.delete(socket);
        this._maybeClosed();
      });
      if (this.maxConnections !== undefined && this._connections.size > this.maxConnections) {
        socket.destroy();
        return;
      }
      internal.dcPublish?.("net.server.socket", { socket });
      this.emit("connection", socket);
    } else if (kind === "close") {
      handles.delete(this._id);
      this._loop.deactivate();
      this._listenerClosed = true;
      this._maybeClosed();
    }
  }

  _maybeClosed() {
    if (this._closing && this._listenerClosed && this._connections.size === 0 && !this._closeEmitted) {
      this._closeEmitted = true;
      this.emit("close");
    }
  }

  address() {
    return this._address;
  }

  close(callback) {
    if (callback) {
      if (!this.listening) {
        const err = Object.assign(new Error("Server is not running."), { code: "ERR_SERVER_NOT_RUNNING" });
        process.nextTick(() => callback(err));
      } else {
        this.once("close", callback);
      }
    }
    if (this.listening) {
      this.listening = false;
      this._closing = true;
      if (this._clusterHandle && this._clusterHandle.rr) {
        // Round-robin cluster worker: the primary owns the listener.
        this._clusterHandle.close();
        this._clusterHandle = null;
        this._listenerClosed = true;
        process.nextTick(() => this._maybeClosed());
      } else {
        if (this._clusterHandle) {
          // Shared cluster handle: tell the primary; the descriptor closes with the listener.
          const h = this._clusterHandle;
          this._clusterHandle = null;
          h.close();
        }
        ops.netServerClose(this._id);
      }
    }
    return this;
  }

  closeAllConnections() {
    for (const socket of this._connections) socket.destroy();
  }

  getConnections(callback) {
    process.nextTick(() => callback(null, this._connections.size));
    return this;
  }

  ref() {
    this._loop.ref();
    return this;
  }
  unref() {
    this._loop.unref();
    return this;
  }
  [Symbol.asyncDispose]() {
    return new Promise((resolve) => this.close(() => resolve()));
  }
}

function createServer(options, connectionListener) {
  return new Server(options, connectionListener);
}

function connect(...args) {
  const { options } = normalizeConnectArgs(args);
  const socket = new Socket(options);
  return socket.connect(...args);
}

const net = {
  Socket,
  Stream: Socket,
  Server,
  createServer,
  connect,
  createConnection: connect,
  isIP,
  isIPv4,
  isIPv6,
  getDefaultAutoSelectFamily: () => false,
  setDefaultAutoSelectFamily() {},
  BlockList: class BlockList {
    constructor() {
      this.rules = [];
    }
    addAddress(address) {
      this.rules.push(address);
    }
    check(address) {
      return this.rules.includes(address);
    }
  },
};

internal.net = net;
internal.defineBuiltin("net", () => net);
