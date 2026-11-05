// node:dgram — UDP sockets over src/ops/dgram.rs, with Node's argument handling, state machine
// (bind/connect queues) and errors.

const EventEmitter = internal.EventEmitter;
const Buffer = internal.Buffer;

const BIND_STATE_UNBOUND = 0;
const BIND_STATE_BINDING = 1;
const BIND_STATE_BOUND = 2;
const CONNECT_STATE_DISCONNECTED = 0;
const CONNECT_STATE_CONNECTING = 1;
const CONNECT_STATE_CONNECTED = 2;

const kState = Symbol("state");
const sockets = new Map();
const sendCallbacks = new Map();
let sendIds = 0;

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

function systemError(info) {
  const err = new Error(info.message);
  for (const k of ["errno", "code", "syscall", "address", "port"]) if (info[k] !== undefined) err[k] = info[k];
  return err;
}

function validatePort(port, name = "Port", allowZero = true) {
  if ((typeof port !== "number" && typeof port !== "string") || (typeof port === "string" && port.trim().length === 0) || +port !== +port >>> 0 || port > 0xffff || (port === 0 && !allowZero)) {
    throw codeError(RangeError, "ERR_SOCKET_BAD_PORT", `${name} should be ${allowZero ? ">=" : ">"} 0 and < 65536. Received ${internal.describeReceived(port).replace(/^type \w+ \((.*)\)$/, "$1")}.`);
  }
  return port | 0;
}

function healthCheck(socket) {
  if (!socket[kState].handle) throw codeError(Error, "ERR_SOCKET_DGRAM_NOT_RUNNING", "Not running");
}

function sliceBuffer(buffer, offset, length) {
  if (typeof buffer === "string") buffer = Buffer.from(buffer);
  else if (!ArrayBuffer.isView(buffer)) {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "buffer" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received ${internal.describeReceived(buffer)}`);
  }
  offset = offset >>> 0;
  length = length >>> 0;
  if (offset > buffer.byteLength) throw codeError(RangeError, "ERR_BUFFER_OUT_OF_BOUNDS", '"offset" is outside of buffer bounds');
  if (offset + length > buffer.byteLength) throw codeError(RangeError, "ERR_BUFFER_OUT_OF_BOUNDS", '"length" is outside of buffer bounds');
  return Buffer.from(buffer.buffer, buffer.byteOffset + offset, length);
}

function fixBufferList(list) {
  const out = new Array(list.length);
  for (let i = 0; i < list.length; i++) {
    const buf = list[i];
    if (typeof buf === "string") out[i] = Buffer.from(buf);
    else if (!ArrayBuffer.isView(buf)) return null;
    else out[i] = Buffer.from(buf.buffer, buf.byteOffset, buf.byteLength);
  }
  return out;
}

function enqueue(socket, toEnqueue) {
  const state = socket[kState];
  if (state.queue === undefined) {
    state.queue = [];
    socket.once(EventEmitter.errorMonitor ?? "error", onListenError);
    socket.once("listening", onListenSuccess);
  }
  state.queue.push(toEnqueue);
}

function onListenSuccess() {
  this.removeListener(EventEmitter.errorMonitor ?? "error", onListenError);
  const queue = this[kState].queue;
  this[kState].queue = undefined;
  for (const fn of queue ?? []) fn();
}

function onListenError() {
  this.removeListener("listening", onListenSuccess);
  this[kState].queue = undefined;
}

internal.dgramEvent = (id, kind, a, b) => sockets.get(id)?._event(kind, a, b);

class Socket extends EventEmitter {
  constructor(type, listener) {
    super();
    let options;
    if (type !== null && typeof type === "object") {
      options = type;
      type = options.type;
    }
    if (type !== "udp4" && type !== "udp6") throw codeError(TypeError, "ERR_SOCKET_BAD_TYPE", "Bad socket type specified. Valid types are: udp4, udp6");
    this.type = type;
    if (typeof listener === "function") this.on("message", listener);
    const handle = ops.dgramCreate(type === "udp6");
    sockets.set(handle, this);
    this[kState] = {
      handle,
      bindState: BIND_STATE_UNBOUND,
      connectState: CONNECT_STATE_DISCONNECTED,
      queue: undefined,
      reuseAddr: options?.reuseAddr,
      reusePort: options?.reusePort,
      ipv6Only: options?.ipv6Only,
      recvBufferSize: options?.recvBufferSize,
      sendBufferSize: options?.sendBufferSize,
      loop: internal.loopHandle(),
    };
    if (options?.signal) {
      const onAbort = () => this.close();
      if (options.signal.aborted) process.nextTick(onAbort);
      else options.signal.addEventListener("abort", onAbort, { once: true });
    }
  }

  _event(kind, a, b) {
    const state = this[kState];
    switch (kind) {
      case "message":
        this.emit("message", Buffer.from(a.buffer, a.byteOffset, a.byteLength), b);
        break;
      case "error":
        this.emit("error", systemError(a));
        break;
      case "sent": {
        const cb = sendCallbacks.get(a);
        sendCallbacks.delete(a);
        if (cb) typeof b === "number" ? cb(null, b) : cb(systemError(b));
        break;
      }
      case "connect":
        if (a !== undefined) {
          state.connectState = CONNECT_STATE_DISCONNECTED;
          this.emit("error", systemError(a));
        } else {
          state.connectState = CONNECT_STATE_CONNECTED;
          this.emit("connect");
        }
        break;
    }
  }

  bind(port_, address_ /* , callback */) {
    let port = port_;
    healthCheck(this);
    const state = this[kState];
    if (state.bindState !== BIND_STATE_UNBOUND) throw codeError(Error, "ERR_SOCKET_ALREADY_BOUND", "Socket is already bound");
    state.bindState = BIND_STATE_BINDING;
    const cb = arguments.length && arguments[arguments.length - 1];
    if (typeof cb === "function") {
      const removeListeners = () => this.removeListener("listening", onListening);
      const onListening = () => {
        this.removeListener("error", removeListeners);
        cb.call(this);
      };
      this.once("error", removeListeners);
      this.once("listening", onListening);
    }
    let address;
    let exclusive;
    if (port !== null && typeof port === "object") {
      address = port.address || "";
      exclusive = Boolean(port.exclusive);
      port = port.port;
    } else {
      address = typeof address_ === "function" ? "" : address_;
      exclusive = false;
    }
    void exclusive;
    if (!address) address = this.type === "udp4" ? "0.0.0.0" : "::";
    port = port === undefined || port === null ? 0 : validatePort(port);
    // Node resolves the address first (asynchronously), then binds.
    setImmediate(() => {
      if (!state.handle) return;
      const r = ops.dgramBind(state.handle, String(address), port, { reuseAddr: state.reuseAddr, reusePort: state.reusePort, ipv6Only: state.ipv6Only });
      if (r.error) {
        state.bindState = BIND_STATE_UNBOUND;
        this.emit("error", systemError(r.error));
        return;
      }
      state.bindState = BIND_STATE_BOUND;
      state.loop.activate();
      if (state.recvBufferSize) this.setRecvBufferSize(state.recvBufferSize);
      if (state.sendBufferSize) this.setSendBufferSize(state.sendBufferSize);
      this.emit("listening");
    });
    return this;
  }

  connect(port, address, callback) {
    port = validatePort(port, "Port", false);
    if (typeof address === "function") {
      callback = address;
      address = "";
    } else if (address === undefined) {
      address = "";
    }
    if (typeof address !== "string") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "address" argument must be of type string. Received ${internal.describeReceived(address)}`);
    const state = this[kState];
    if (state.connectState !== CONNECT_STATE_DISCONNECTED) throw codeError(Error, "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected");
    state.connectState = CONNECT_STATE_CONNECTING;
    if (state.bindState === BIND_STATE_UNBOUND) this.bind({ port: 0, exclusive: true }, null);
    if (state.bindState !== BIND_STATE_BOUND) {
      enqueue(this, () => this._doConnect(port, address, callback));
      return;
    }
    this._doConnect(port, address, callback);
  }

  _doConnect(port, address, callback) {
    const state = this[kState];
    if (!state.handle) return;
    if (!address) address = this.type === "udp4" ? "127.0.0.1" : "::1";
    if (typeof callback === "function") this.once("connect", callback);
    ops.dgramConnect(state.handle, address, port);
  }

  disconnect() {
    const state = this[kState];
    if (state.connectState !== CONNECT_STATE_CONNECTED) throw codeError(Error, "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected");
    ops.dgramDisconnect(state.handle);
    state.connectState = CONNECT_STATE_DISCONNECTED;
  }

  send(buffer, offset, length, port, address, callback) {
    let list;
    const state = this[kState];
    const connected = state.connectState === CONNECT_STATE_CONNECTED;
    if (!connected) {
      if (address || (port && typeof port !== "function")) {
        buffer = sliceBuffer(buffer, offset, length);
      } else {
        callback = port;
        port = offset;
        address = length;
      }
    } else {
      if (typeof length === "number") {
        buffer = sliceBuffer(buffer, offset, length);
        if (typeof port === "function") {
          callback = port;
          port = null;
        }
      } else {
        callback = offset;
      }
      if (port || address) throw codeError(Error, "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected");
    }
    if (!Array.isArray(buffer)) {
      if (typeof buffer === "string") list = [Buffer.from(buffer)];
      else if (!ArrayBuffer.isView(buffer)) {
        throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "buffer" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received ${internal.describeReceived(buffer)}`);
      } else list = [Buffer.from(buffer.buffer, buffer.byteOffset, buffer.byteLength)];
    } else if (!(list = fixBufferList(buffer))) {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "buffer list arguments" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received ${internal.describeReceived(buffer)}`);
    }
    if (!connected) port = validatePort(port, "Port", false);
    if (typeof callback !== "function") callback = undefined;
    if (typeof address === "function") {
      callback = address;
      address = undefined;
    } else if (address != null && typeof address !== "string") {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "address" argument must be of type string. Received ${internal.describeReceived(address)}`);
    }
    healthCheck(this);
    if (state.bindState === BIND_STATE_UNBOUND) this.bind({ port: 0, exclusive: true }, null);
    if (list.length === 0) list.push(Buffer.alloc(0));
    if (state.bindState !== BIND_STATE_BOUND) {
      enqueue(this, () => this.send(list, port, address, callback));
      return;
    }
    const doSend = (ip) => {
      // The socket may have been closed while the address was looked up.
      if (!state.handle) return;
      const id = ++sendIds;
      const r = ops.dgramSend(state.handle, Buffer.concat(list), connected ? null : ip, connected ? 0 : port, id);
      if (r === undefined) {
        if (callback) sendCallbacks.set(id, callback);
      } else if (callback) {
        if (typeof r === "number") process.nextTick(callback, null, r);
        else process.nextTick(callback, systemError(r));
      }
    };
    if (connected) {
      doSend(null);
      return;
    }
    // Like Node: look the address up first (dns.lookup answers IP literals on the next tick).
    const host = address || (this.type === "udp4" ? "127.0.0.1" : "::1");
    const family = this.type === "udp4" ? 4 : 6;
    if (internal.requireBuiltin("net").isIP(host)) {
      process.nextTick(doSend, host);
      return;
    }
    internal.requireBuiltin("dns").lookup(host, { family }, (err, ip) => {
      if (err) {
        err.syscall = "getaddrinfo";
        if (callback) callback(err);
        else this.emit("error", err);
        return;
      }
      doSend(ip);
    });
  }

  close(callback) {
    const state = this[kState];
    const queue = state.queue;
    if (typeof callback === "function") this.on("close", callback);
    if (queue !== undefined) {
      queue.push(() => this.close());
      return this;
    }
    healthCheck(this);
    sockets.delete(state.handle);
    ops.dgramClose(state.handle);
    state.handle = null;
    state.loop.deactivate();
    process.nextTick(() => this.emit("close"));
    return this;
  }

  [Symbol.asyncDispose]() {
    if (!this[kState].handle) return Promise.resolve();
    return new Promise((resolve) => this.close(resolve));
  }

  address() {
    healthCheck(this);
    const r = ops.dgramAddress(this[kState].handle, false);
    if (r.error) throw systemError(r.error);
    return r;
  }

  remoteAddress() {
    healthCheck(this);
    const state = this[kState];
    if (state.connectState !== CONNECT_STATE_CONNECTED) throw codeError(Error, "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected");
    const r = ops.dgramAddress(state.handle, true);
    if (r.error) throw systemError(r.error);
    return r;
  }

  _option(name, a, b) {
    healthCheck(this);
    const r = ops.dgramOption(this[kState].handle, name, a, b);
    if (r !== null && typeof r === "object" && r.error) throw systemError(r.error);
    return r;
  }

  setBroadcast(arg) {
    this._option("broadcast", Boolean(arg));
  }
  setTTL(ttl) {
    if (typeof ttl !== "number") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "ttl" argument must be of type number. Received ${internal.describeReceived(ttl)}`);
    return this._option("ttl", ttl);
  }
  setMulticastTTL(ttl) {
    if (typeof ttl !== "number") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "ttl" argument must be of type number. Received ${internal.describeReceived(ttl)}`);
    return this._option("multicastTTL", ttl);
  }
  setMulticastLoopback(arg) {
    this._option("multicastLoopback", Boolean(arg));
    return arg;
  }
  setMulticastInterface(interfaceAddress) {
    this._option("multicastInterface", String(interfaceAddress));
  }
  addMembership(multicastAddress, interfaceAddress) {
    if (!multicastAddress) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "multicastAddress" argument must be specified');
    this._option("addMembership", String(multicastAddress), interfaceAddress);
  }
  dropMembership(multicastAddress, interfaceAddress) {
    if (!multicastAddress) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "multicastAddress" argument must be specified');
    this._option("dropMembership", String(multicastAddress), interfaceAddress);
  }
  addSourceSpecificMembership() {
    throw codeError(Error, "ERR_METHOD_NOT_IMPLEMENTED", "The addSourceSpecificMembership() method is not implemented");
  }
  dropSourceSpecificMembership() {
    throw codeError(Error, "ERR_METHOD_NOT_IMPLEMENTED", "The dropSourceSpecificMembership() method is not implemented");
  }
  setRecvBufferSize(size) {
    this._option("recvBufferSize", size);
  }
  setSendBufferSize(size) {
    this._option("sendBufferSize", size);
  }
  getRecvBufferSize() {
    return this._option("getRecvBufferSize");
  }
  getSendBufferSize() {
    return this._option("getSendBufferSize");
  }
  getSendQueueSize() {
    return 0;
  }
  getSendQueueCount() {
    return 0;
  }
  ref() {
    this[kState].loop.ref();
    return this;
  }
  unref() {
    this[kState].loop.unref();
    return this;
  }
}

function createSocket(type, listener) {
  return new Socket(type, listener);
}

internal.defineBuiltin("dgram", () => ({ createSocket, Socket }));
