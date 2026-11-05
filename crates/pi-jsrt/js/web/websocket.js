// WebSocket (WHATWG, as Node 22's undici implements it) over native tokio-tungstenite ops.
// Loaded on first access to the `WebSocket` global.

const { Event, EventTarget, DOMException } = internal;
const sockets = new Map();

const CONNECTING = 0;
const OPEN = 1;
const CLOSING = 2;
const CLOSED = 3;

class CloseEvent extends Event {
  #init;
  constructor(type, init = {}) {
    super(type, init);
    this.#init = { wasClean: Boolean(init?.wasClean), code: Number(init?.code ?? 0), reason: String(init?.reason ?? "") };
  }
  get wasClean() {
    return this.#init.wasClean;
  }
  get code() {
    return this.#init.code;
  }
  get reason() {
    return this.#init.reason;
  }
}

class ErrorEvent extends Event {
  #init;
  constructor(type, init = {}) {
    super(type, init);
    this.#init = { message: String(init?.message ?? ""), filename: String(init?.filename ?? ""), lineno: init?.lineno ?? 0, colno: init?.colno ?? 0, error: init?.error };
  }
  get message() {
    return this.#init.message;
  }
  get filename() {
    return this.#init.filename;
  }
  get lineno() {
    return this.#init.lineno;
  }
  get colno() {
    return this.#init.colno;
  }
  get error() {
    return this.#init.error;
  }
}

const utf8Length = (s) => internal.Buffer.byteLength(s, "utf8");
const validProtocol = (p) => /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(p);

function defineHandler(name) {
  const key = Symbol(name);
  Object.defineProperty(WebSocket.prototype, `on${name}`, {
    configurable: true,
    enumerable: true,
    get() {
      return this[key] ?? null;
    },
    // EventTarget's dispatchEvent calls `this.on<type>` itself.
    set(fn) {
      this[key] = typeof fn === "function" ? fn : null;
    },
  });
}

class WebSocket extends EventTarget {
  #id;
  #url;
  #readyState = CONNECTING;
  #protocol = "";
  #extensions = "";
  #binaryType = "blob";
  #buffered = 0;
  #sendChain = null;
  #origin;

  constructor(url, protocols = []) {
    super();
    if (arguments.length === 0) throw new TypeError("Failed to construct 'WebSocket': 1 argument required, but only 0 present.");
    let headers;
    if (protocols !== null && typeof protocols === "object" && !Array.isArray(protocols) && !(Symbol.iterator in protocols)) {
      headers = protocols.headers;
      protocols = protocols.protocols ?? [];
    }
    let parsed;
    try {
      parsed = new internal.URL(String(url));
    } catch (e) {
      throw new DOMException(String(e), "SyntaxError");
    }
    if (parsed.protocol === "http:") parsed.protocol = "ws:";
    else if (parsed.protocol === "https:") parsed.protocol = "wss:";
    if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
      throw new DOMException(`Expected a ws: or wss: protocol, got ${parsed.protocol}`, "SyntaxError");
    }
    if (parsed.hash || parsed.href.endsWith("#")) throw new DOMException("Got fragment", "SyntaxError");
    if (typeof protocols === "string") protocols = [protocols];
    protocols = Array.from(protocols, String);
    if (protocols.length !== new Set(protocols.map((p) => p.toLowerCase())).size || !protocols.every(validProtocol)) {
      throw new DOMException("Invalid Sec-WebSocket-Protocol value", "SyntaxError");
    }
    this.#url = parsed.href;
    this.#origin = parsed.origin;
    const pairs = [];
    if (headers) {
      const entries = typeof headers.entries === "function" && !Array.isArray(headers) ? headers.entries() : Array.isArray(headers) ? headers : Object.entries(headers);
      for (const [k, v] of entries) pairs.push(String(k), String(v));
    }
    this.#id = ops.wsConnect(this.#url, protocols, pairs);
    sockets.set(this.#id, this);
  }

  get url() {
    return this.#url;
  }
  get readyState() {
    return this.#readyState;
  }
  get bufferedAmount() {
    return this.#buffered;
  }
  get protocol() {
    return this.#protocol;
  }
  get extensions() {
    return this.#extensions;
  }
  get binaryType() {
    return this.#binaryType;
  }
  set binaryType(type) {
    if (type === "blob" || type === "arraybuffer") this.#binaryType = type;
  }

  send(data) {
    if (arguments.length === 0) throw new TypeError("Failed to execute 'send' on 'WebSocket': 1 argument required, but only 0 present.");
    if (this.#readyState === CONNECTING) throw new DOMException("Sent before connected.", "InvalidStateError");
    if (this.#readyState !== OPEN) return;
    const Blob = globalThis.Blob;
    if (typeof data === "string") {
      this.#enqueue(() => data, utf8Length(data), false);
    } else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
      const bytes = data instanceof ArrayBuffer ? new Uint8Array(data.slice(0)) : new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
      this.#enqueue(() => bytes, bytes.byteLength, true);
    } else if (Blob && data instanceof Blob) {
      const p = data.arrayBuffer().then((ab) => new Uint8Array(ab));
      this.#enqueue(() => p, data.size, true);
    } else {
      const text = String(data);
      this.#enqueue(() => text, utf8Length(text), false);
    }
  }

  // Messages go out in order; a Blob is read asynchronously first, and later messages wait for it.
  #enqueue(get, size, binary) {
    this.#buffered += size;
    const value = get();
    if (!this.#sendChain && !(value instanceof Promise)) {
      ops.wsSend(this.#id, value, binary);
      return;
    }
    const prev = this.#sendChain ?? Promise.resolve();
    const next = prev.then(() => value).then((v) => {
      if (this.#readyState === OPEN || this.#readyState === CLOSING) ops.wsSend(this.#id, v, binary);
    });
    this.#sendChain = next;
    next.then(() => {
      if (this.#sendChain === next) this.#sendChain = null;
    });
  }

  close(code = undefined, reason = undefined) {
    if (code !== undefined) {
      code = Number(code);
      if (!(code === 1000 || (code >= 3000 && code <= 4999)) || !Number.isInteger(code)) {
        throw new DOMException("invalid code", "InvalidAccessError");
      }
    }
    if (reason !== undefined) {
      reason = String(reason);
      const n = utf8Length(reason);
      if (n > 123) throw new DOMException(`Reason must be less than 123 bytes; received ${n}`, "SyntaxError");
    }
    if (this.#readyState === CLOSING || this.#readyState === CLOSED) return;
    if (this.#readyState === CONNECTING) {
      // Fail the connection: no close frame, the close event reports an abnormal closure.
      this.#readyState = CLOSING;
      this.#abortPending = true;
      return;
    }
    this.#readyState = CLOSING;
    const send = () => ops.wsClose(this.#id, code ?? (reason ? 1000 : undefined), reason);
    if (this.#sendChain) this.#sendChain.then(send);
    else send();
  }

  #abortPending = false;

  _event(kind, a, b, c) {
    switch (kind) {
      case "open":
        if (this.#abortPending) {
          ops.wsClose(this.#id, undefined, undefined);
          return;
        }
        this.#readyState = OPEN;
        this.#protocol = a;
        this.#extensions = b;
        this.dispatchEvent(new Event("open"));
        break;
      case "sent":
        this.#buffered = Math.max(0, this.#buffered - a);
        break;
      case "text":
      case "binary": {
        if (this.#readyState !== OPEN) return;
        let data = a;
        if (kind === "binary") {
          data = this.#binaryType === "arraybuffer" ? a.buffer.slice(a.byteOffset, a.byteOffset + a.byteLength) : new globalThis.Blob([a]);
        }
        this.dispatchEvent(new globalThis.MessageEvent("message", { data, origin: this.#origin }));
        break;
      }
      case "fail": {
        sockets.delete(this.#id);
        // Node 22 (undici 6): an error event; readyState stays CONNECTING and no close event.
        const message = "Received network error or non-101 status code.";
        if (this.#abortPending) {
          this.#readyState = CLOSED;
          this.dispatchEvent(new CloseEvent("close", { wasClean: false, code: 1006, reason: "" }));
          return;
        }
        this.dispatchEvent(new ErrorEvent("error", { error: new Error(message), message }));
        break;
      }
      case "close":
        sockets.delete(this.#id);
        this.#readyState = CLOSED;
        if (this.#abortPending) {
          a = 1006;
          b = "";
          c = false;
        }
        this.dispatchEvent(new CloseEvent("close", { wasClean: c, code: a, reason: b }));
        break;
    }
  }

  get [Symbol.toStringTag]() {
    return "WebSocket";
  }
}

for (const [name, value] of Object.entries({ CONNECTING, OPEN, CLOSING, CLOSED })) {
  Object.defineProperty(WebSocket, name, { value, enumerable: true });
  Object.defineProperty(WebSocket.prototype, name, { value, enumerable: true });
}
for (const name of ["open", "error", "close", "message"]) defineHandler(name);

internal.wsEvent = (id, kind, a, b, c) => sockets.get(id)?._event(kind, a, b, c);
internal.CloseEvent = CloseEvent;
internal.ErrorEvent = ErrorEvent;
internal.hidden(globalThis, "WebSocket", WebSocket);
