// MessageEvent, MessageChannel/MessagePort and BroadcastChannel (ports are entries of the
// process-wide registry in src/ops/worker.rs, so they work across worker threads), and
// `navigator`. Loaded on first access to one of these globals.

const { Event, EventTarget } = internal;
internal.requireBuiltin("v8");

class MessageEvent extends Event {
  #data;
  #origin;
  #lastEventId;
  #source;
  #ports;
  constructor(type, init = {}) {
    super(type, init);
    this.#data = init?.data === undefined ? null : init.data;
    this.#origin = init?.origin === undefined ? "" : String(init.origin);
    this.#lastEventId = init?.lastEventId === undefined ? "" : String(init.lastEventId);
    this.#source = init?.source ?? null;
    this.#ports = Object.freeze([...(init?.ports ?? [])]);
  }
  get data() {
    return this.#data;
  }
  get origin() {
    return this.#origin;
  }
  get lastEventId() {
    return this.#lastEventId;
  }
  get source() {
    return this.#source;
  }
  get ports() {
    return this.#ports;
  }
}

function defineHandler(proto, name) {
  const key = Symbol(name);
  Object.defineProperty(proto, `on${name}`, {
    configurable: true,
    enumerable: true,
    get() {
      return this[key] ?? null;
    },
    // EventTarget's dispatchEvent calls `this.on<type>` itself.
    set(fn) {
      this[key] = typeof fn === "function" ? fn : null;
      if (this[key] && name === "message") this.start?.();
    },
  });
}

const kCreate = Symbol("create");
const kWrapped = Symbol("wrapped");
const localPorts = new Map(); // registry id -> MessagePort owned by this runtime

function dataCloneError(message) {
  return new internal.DOMException(message, "DataCloneError");
}

// Serialize `message` for a port or worker: V8 format with transferred ArrayBuffers and
// MessagePorts (host objects referring to the transferred port ids).
function serializeMessage(message, transferList, source) {
  transferList = transferList == null ? [] : Array.from(transferList);
  const transferPorts = [];
  const seenBuffers = new Set();
  for (const t of transferList) {
    if (t instanceof MessagePort) {
      if (t === source) throw dataCloneError("Transfer list contains source port");
      if (t._detached) throw dataCloneError("MessagePort in transfer list is already detached");
      if (!transferPorts.includes(t)) transferPorts.push(t);
    } else if (t instanceof ArrayBuffer) {
      if (seenBuffers.has(t)) throw dataCloneError("Transfer list contains duplicate ArrayBuffer");
      seenBuffers.add(t);
    }
  }
  const { bytes, ports, buffers } = internal.cloneSerialize(message, transferList, {
    isHost: (v) => v instanceof MessagePort,
    writeHost(v, ids) {
      if (!transferPorts.includes(v)) throw dataCloneError("Object that needs transfer was found in message but not listed in transferList");
      let i = ids.indexOf(v._id);
      if (i < 0) i = ids.push(v._id) - 1;
      return i;
    },
  });
  for (const p of transferPorts) if (!ports.includes(p._id)) ports.push(p._id);
  for (const p of transferPorts) p._detach();
  return { bytes, ports, buffers };
}

function adoptPort(id) {
  const existing = localPorts.get(id);
  if (existing) return existing;
  const port = new MessagePort(kCreate, id);
  ops.portAttach(id);
  return port;
}

function deserializeMessage(bytes, portIds, buffers) {
  const ports = portIds.map(adoptPort);
  const data = internal.cloneDeserialize(bytes, buffers, { readHost: (i) => ports[i] });
  return { data, ports };
}
internal.serializeMessage = serializeMessage;
internal.deserializeMessage = deserializeMessage;

class MessagePort extends EventTarget {
  #id;
  #started = false;
  #closed = false;
  #detached = false;
  #refed = true;
  #counted = false;
  constructor(token, id) {
    if (token !== kCreate) throw new TypeError("Illegal constructor");
    super();
    this.#id = id;
    localPorts.set(id, this);
  }
  get _id() {
    return this.#id;
  }
  get _detached() {
    return this.#detached || this.#closed;
  }
  #updateRef() {
    const want = this.#started && !this.#closed && !this.#detached && this.#refed;
    if (want !== this.#counted) {
      this.#counted = want;
      ops.loopRef(want ? 1 : -1);
    }
  }
  _detach() {
    this.#detached = true;
    localPorts.delete(this.#id);
    ops.portDetach(this.#id);
    this.#updateRef();
  }
  postMessage(message, transfer) {
    if (this.#closed || this.#detached) return;
    const transferList = Array.isArray(transfer) ? transfer : transfer?.transfer;
    const { bytes, ports, buffers } = serializeMessage(message, transferList, this);
    ops.portSend(this.#id, bytes, ports, buffers);
  }
  // Deliver one raw message from the registry queue.
  _deliver(raw) {
    const [bytes, portIds, buffers] = raw;
    let msg;
    try {
      msg = deserializeMessage(bytes, portIds, buffers);
    } catch (error) {
      this.dispatchEvent(new MessageEvent("messageerror", { data: error }));
      return;
    }
    this.dispatchEvent(new MessageEvent("message", { data: msg.data, ports: msg.ports }));
  }
  _drain() {
    while (this.#started && !this.#closed && !this.#detached) {
      const raw = ops.portRecv(this.#id);
      if (raw === undefined) break;
      this._deliver(raw);
    }
  }
  _receiveOne() {
    const raw = ops.portRecv(this.#id);
    if (raw === undefined) return undefined;
    const [bytes, portIds, buffers] = raw;
    return { message: deserializeMessage(bytes, portIds, buffers).data };
  }
  _onClose() {
    if (this.#closed) return;
    this.#closed = true;
    localPorts.delete(this.#id);
    this.#updateRef();
    this.dispatchEvent(new Event("close"));
  }
  start() {
    if (this.#started || this.#closed) return;
    this.#started = true;
    this.#updateRef();
    ops.portWake(this.#id);
  }
  close() {
    if (this.#closed || this.#detached) return;
    // Messages already queued are still delivered (Node drains before closing).
    this._drain();
    ops.portClose(this.#id);
  }
  ref() {
    this.#refed = true;
    this.#updateRef();
    return this;
  }
  unref() {
    this.#refed = false;
    this.#updateRef();
    return this;
  }
  hasRef() {
    return this.#refed && this.#started && !this.#closed;
  }
  addEventListener(name, fn, options) {
    super.addEventListener(name, fn, options);
    if (name === "message") this.start();
  }
  // Node's EventEmitter-style API on ports.
  on(name, fn) {
    if (name === "message" || name === "messageerror") {
      const wrapped = (e) => fn(e.data);
      fn[kWrapped] = wrapped;
      this.addEventListener(name, wrapped);
    } else {
      this.addEventListener(name, fn);
    }
    return this;
  }
  once(name, fn) {
    const wrapped = name === "message" || name === "messageerror" ? (e) => fn(e.data) : fn;
    fn[kWrapped] = wrapped;
    this.addEventListener(name, wrapped, { once: true });
    return this;
  }
  off(name, fn) {
    this.removeEventListener(name, fn[kWrapped] ?? fn);
    return this;
  }
  removeListener(name, fn) {
    return this.off(name, fn);
  }
  addListener(name, fn) {
    return this.on(name, fn);
  }
  removeAllListeners(name) {
    internal.clearEventListeners?.(this, name);
    return this;
  }
  emit(name, ...args) {
    if (name === "message" || name === "messageerror") return this.dispatchEvent(new MessageEvent(name, { data: args[0] }));
    return this.dispatchEvent(new Event(name));
  }
  get [Symbol.toStringTag]() {
    return "MessagePort";
  }
}
defineHandler(MessagePort.prototype, "message");
defineHandler(MessagePort.prototype, "messageerror");

internal.portEvent = (id, kind) => {
  const port = localPorts.get(id);
  if (!port) return;
  if (kind === "wake") port._drain();
  else if (kind === "close") port._onClose();
  else if (kind === "peerClosed") {
    port._drain();
    ops.portClose(id);
    port._onClose();
  }
};
internal.createPort = (id) => adoptPort(id);
internal.receiveMessageOnPort = (port) => {
  if (!(port instanceof MessagePort)) {
    const err = new TypeError(`The "port" argument must be a MessagePort instance`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  return port._receiveOne();
};

class MessageChannel {
  #port1;
  #port2;
  constructor() {
    const [a, b] = ops.portCreatePair();
    this.#port1 = new MessagePort(kCreate, a);
    this.#port2 = new MessagePort(kCreate, b);
  }
  get port1() {
    return this.#port1;
  }
  get port2() {
    return this.#port2;
  }
}

const broadcasts = new Map(); // subscriber id -> BroadcastChannel

class BroadcastChannel extends EventTarget {
  #name;
  #id;
  #closed = false;
  #refed = true;
  constructor(name) {
    if (arguments.length === 0) throw new TypeError('The "name" argument must be specified');
    super();
    this.#name = String(name);
    this.#id = ops.broadcastJoin(this.#name);
    broadcasts.set(this.#id, this);
    ops.loopRef(1);
  }
  get name() {
    return this.#name;
  }
  postMessage(message) {
    if (arguments.length === 0) throw new TypeError('The "message" argument must be specified');
    if (this.#closed) throw new internal.DOMException("BroadcastChannel is closed.", "InvalidStateError");
    const { bytes } = internal.cloneSerialize(message, []);
    ops.broadcastPost(this.#name, this.#id, bytes);
  }
  _deliver(bytes) {
    if (this.#closed) return;
    let data;
    try {
      data = internal.cloneDeserialize(bytes, []);
    } catch (error) {
      this.dispatchEvent(new MessageEvent("messageerror", { data: error }));
      return;
    }
    this.dispatchEvent(new MessageEvent("message", { data }));
  }
  close() {
    if (this.#closed) return;
    this.#closed = true;
    broadcasts.delete(this.#id);
    ops.broadcastLeave(this.#name, this.#id);
    if (this.#refed) ops.loopRef(-1);
  }
  ref() {
    if (!this.#refed && !this.#closed) ops.loopRef(1);
    this.#refed = true;
    return this;
  }
  unref() {
    if (this.#refed && !this.#closed) ops.loopRef(-1);
    this.#refed = false;
    return this;
  }
  get [Symbol.toStringTag]() {
    return "BroadcastChannel";
  }
}
defineHandler(BroadcastChannel.prototype, "message");
defineHandler(BroadcastChannel.prototype, "messageerror");
internal.broadcastEvent = (id, bytes) => broadcasts.get(id)?._deliver(bytes);

class Navigator {
  constructor(token) {
    if (token !== kCreate) throw new TypeError("Illegal constructor");
  }
  get hardwareConcurrency() {
    return internal.requireBuiltin("os").availableParallelism?.() ?? internal.requireBuiltin("os").cpus().length;
  }
  get language() {
    return "en-US";
  }
  get languages() {
    return ["en-US"];
  }
  get platform() {
    return process.platform;
  }
  get userAgent() {
    return `Node.js/${process.version.slice(1).split(".")[0]}`;
  }
  get [Symbol.toStringTag]() {
    return "Navigator";
  }
}

internal.messaging = { MessageEvent, MessagePort, MessageChannel, BroadcastChannel };
globalThis.MessageEvent = MessageEvent;
globalThis.MessagePort = MessagePort;
globalThis.MessageChannel = MessageChannel;
globalThis.BroadcastChannel = BroadcastChannel;
globalThis.Navigator = Navigator;
globalThis.navigator = new Navigator(kCreate);
